use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::protocol::{encode_data, MessageKind, ParseEvent, Parser};

/// ANSI escape sequence parser state.
#[derive(Debug, Clone, Copy, PartialEq)]
enum AnsiState {
    Normal,
    /// Seen ESC (0x1b), waiting for `[`.
    Esc,
    /// Inside a CSI sequence (ESC [ ...), accumulating parameter bytes.
    Csi,
}

/// Tracks line boundaries in a byte stream and inserts timestamps at the
/// start of each new line. Preserves ANSI color state across line breaks
/// so timestamps render without colors but device output keeps them.
pub struct LineTimestamper {
    at_line_start: bool,
    ansi_state: AnsiState,
    /// Buffer for CSI parameter bytes being parsed.
    csi_buf: Vec<u8>,
    /// Accumulated SGR sequences since last reset, replayed after each timestamp.
    active_sgr: Vec<u8>,
}

impl LineTimestamper {
    pub fn new() -> Self {
        Self {
            at_line_start: true,
            ansi_state: AnsiState::Normal,
            csi_buf: Vec::new(),
            active_sgr: Vec::new(),
        }
    }

    /// Process a buffer of bytes, inserting timestamps at line starts.
    /// Returns the output bytes to write to the terminal.
    pub fn process(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len() + 128);
        for &byte in input {
            if self.at_line_start && byte != b'\r' && byte != b'\n' {
                output.extend_from_slice(b"\x1b[0m");
                let ts = crate::server::log_writer::format_timestamp();
                output.extend_from_slice(ts.as_bytes());
                output.extend_from_slice(&self.active_sgr);
                self.at_line_start = false;
            }

            // Track ANSI escape sequences for SGR state
            match self.ansi_state {
                AnsiState::Normal => {
                    if byte == 0x1b {
                        self.ansi_state = AnsiState::Esc;
                    }
                }
                AnsiState::Esc => {
                    if byte == b'[' {
                        self.ansi_state = AnsiState::Csi;
                        self.csi_buf.clear();
                    } else {
                        self.ansi_state = AnsiState::Normal;
                    }
                }
                AnsiState::Csi => {
                    if (0x40..=0x7E).contains(&byte) {
                        // Final byte — CSI sequence complete
                        if byte == b'm' {
                            self.handle_sgr();
                        }
                        self.ansi_state = AnsiState::Normal;
                    } else {
                        self.csi_buf.push(byte);
                    }
                }
            }

            output.push(byte);
            if byte == b'\n' {
                self.at_line_start = true;
            }
        }
        output
    }

    /// Update tracked SGR state from the completed CSI sequence in `csi_buf`.
    ///
    /// Handles compound params like `\x1b[0;31m` — if any param is a reset (0),
    /// prior state is cleared and only the non-reset params after it are kept.
    fn handle_sgr(&mut self) {
        if self.csi_buf.is_empty() {
            // \x1b[m — implicit reset
            self.active_sgr.clear();
            return;
        }

        // Split on `;` to handle compound params like `0;1;31`
        let params: Vec<&[u8]> = self.csi_buf.split(|&b| b == b';').collect();

        // Find the last reset param (0 or 00) — everything before it is irrelevant
        let last_reset = params.iter().rposition(|p| p.is_empty() || *p == b"0" || *p == b"00");

        if let Some(reset_idx) = last_reset {
            // Clear prior state
            self.active_sgr.clear();
            // Keep only params after the last reset
            let remaining: Vec<&[u8]> = params[reset_idx + 1..].to_vec();
            if !remaining.is_empty() {
                self.active_sgr.extend_from_slice(b"\x1b[");
                for (i, param) in remaining.iter().enumerate() {
                    if i > 0 {
                        self.active_sgr.push(b';');
                    }
                    self.active_sgr.extend_from_slice(param);
                }
                self.active_sgr.push(b'm');
            }
        } else {
            // Safety cap: stop accumulating if active_sgr is already large.
            // Prevents unbounded growth from devices that never emit a reset.
            // A real reset will still clear it.
            if self.active_sgr.len() > 256 {
                return;
            }
            self.active_sgr.extend_from_slice(b"\x1b[");
            self.active_sgr.extend_from_slice(&self.csi_buf);
            self.active_sgr.push(b'm');
        }
    }
}

/// Run interactive mode with raw terminal I/O using channel-based transport.
///
/// Reads incoming data from `data_rx` and displays it on the terminal.
/// Keyboard input is forwarded via `data_tx`. Ctrl+] quits.
pub async fn run_interactive(
    mut data_rx: mpsc::Receiver<Vec<u8>>,
    data_tx: mpsc::Sender<Vec<u8>>,
    timestamps: bool,
) -> anyhow::Result<()> {
    crossterm::terminal::enable_raw_mode()?;

    let result = async {
        use crossterm::event::{Event, EventStream};
        use futures::StreamExt;

        let mut event_stream = EventStream::new();
        let mut escape_state = EscapeState::Normal;
        let mut timestamper = LineTimestamper::new();
        let mut stdout = tokio::io::stdout();

        loop {
            tokio::select! {
                data = data_rx.recv() => {
                    match data {
                        Some(bytes) => {
                            if timestamps {
                                let stamped = timestamper.process(&bytes);
                                stdout.write_all(&stamped).await?;
                            } else {
                                stdout.write_all(&bytes).await?;
                            }
                            stdout.flush().await?;
                        }
                        None => {
                            eprintln!("\r\nConnection lost");
                            break;
                        }
                    }
                }
                event = event_stream.next() => {
                    match event {
                        Some(Ok(Event::Key(key_event))) => {
                            match handle_key(key_event, &mut escape_state) {
                                KeyAction::Quit => break,
                                KeyAction::SendByte(b) => {
                                    let _ = data_tx.send(vec![b]).await;
                                }
                                KeyAction::SendBytes(bytes) => {
                                    let _ = data_tx.send(bytes).await;
                                }
                                KeyAction::None => {}
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            eprintln!("\r\nInput error: {e}");
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }
    .await;

    crossterm::terminal::disable_raw_mode()?;
    println!();

    result
}

/// Run interactive mode over any async stream with protocol framing.
///
/// Sets up bridge tasks between the stream (using the rmux protocol) and
/// the channel-based `run_interactive`, then cleans up on return.
pub async fn run_interactive_socket(
    stream: impl AsyncRead + AsyncWrite + Send + 'static,
    timestamps: bool,
) -> anyhow::Result<()> {
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let (incoming_tx, incoming_rx) = mpsc::channel::<Vec<u8>>(256);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Vec<u8>>(256);

    // Reader bridge: socket → protocol parser → incoming channel
    let reader_handle = tokio::spawn(async move {
        let mut parser = Parser::new();
        let mut buf = [0u8; 4096];
        loop {
            match read_half.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let events = parser.feed_bytes(&buf[..n]);
                    let mut data_buf = Vec::new();
                    for event in events {
                        match event {
                            ParseEvent::DataByte(b) => data_buf.push(b),
                            ParseEvent::MalformedEscape(b) => {
                                data_buf.push(0x00);
                                data_buf.push(b);
                            }
                            ParseEvent::Message {
                                kind: MessageKind::Response,
                                json,
                            } => {
                                // Suppress internal history_end events
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                                    if v.get("event").and_then(|v| v.as_str())
                                        == Some("history_end")
                                    {
                                        continue;
                                    }
                                }
                                let msg = format!("\r\n[server: {json}]\r\n");
                                if incoming_tx.send(msg.into_bytes()).await.is_err() {
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                    if !data_buf.is_empty() && incoming_tx.send(data_buf).await.is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Writer bridge: outgoing channel → encode_data → socket
    let writer_handle = tokio::spawn(async move {
        while let Some(data) = outgoing_rx.recv().await {
            let encoded = encode_data(&data);
            if write_half.write_all(&encoded).await.is_err() {
                break;
            }
        }
    });

    let result = run_interactive(incoming_rx, outgoing_tx, timestamps).await;

    reader_handle.abort();
    writer_handle.abort();

    result
}

/// Escape sequence state machine for keyboard shortcuts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EscapeState {
    Normal,
    /// Received Ctrl+T, waiting for next key
    CtrlT,
}

/// Action to take based on a key event.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyAction {
    Quit,
    SendByte(u8),
    SendBytes(Vec<u8>),
    None,
}

/// Process a key event through the escape state machine.
pub fn handle_key(
    key: crossterm::event::KeyEvent,
    state: &mut EscapeState,
) -> KeyAction {
    use crossterm::event::{KeyCode, KeyModifiers};

    match *state {
        EscapeState::Normal => {
            // Ctrl+] → quit
            if key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return KeyAction::Quit;
            }
            // Ctrl+T → enter escape prefix
            if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
                *state = EscapeState::CtrlT;
                return KeyAction::None;
            }
            // Regular key — forward to serial
            key_to_action(key)
        }
        EscapeState::CtrlT => {
            *state = EscapeState::Normal;
            // Q → quit
            if key.code == KeyCode::Char('q') || key.code == KeyCode::Char('Q') {
                return KeyAction::Quit;
            }
            // Ctrl+T → send literal Ctrl+T
            if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return KeyAction::SendByte(0x14); // Ctrl+T
            }
            // Anything else → swallow the prefix and forward the key
            key_to_action(key)
        }
    }
}

/// Convert a key event to a send action.
fn key_to_action(key: crossterm::event::KeyEvent) -> KeyAction {
    use crossterm::event::{KeyCode, KeyModifiers};

    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+letter → ASCII control code
                let ctrl_byte = (c as u8).wrapping_sub(b'`');
                KeyAction::SendByte(ctrl_byte)
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                if s.len() == 1 {
                    KeyAction::SendByte(s.as_bytes()[0])
                } else {
                    KeyAction::SendBytes(s.as_bytes().to_vec())
                }
            }
        }
        KeyCode::Enter => KeyAction::SendByte(b'\r'),
        KeyCode::Backspace => KeyAction::SendByte(0x7F),
        KeyCode::Tab => KeyAction::SendByte(b'\t'),
        KeyCode::Esc => KeyAction::SendByte(0x1B),
        KeyCode::Up => KeyAction::SendBytes(b"\x1b[A".to_vec()),
        KeyCode::Down => KeyAction::SendBytes(b"\x1b[B".to_vec()),
        KeyCode::Right => KeyAction::SendBytes(b"\x1b[C".to_vec()),
        KeyCode::Left => KeyAction::SendBytes(b"\x1b[D".to_vec()),
        _ => KeyAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn ctrl_bracket_quits() {
        let mut state = EscapeState::Normal;
        let action = handle_key(
            make_key(KeyCode::Char(']'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_eq!(action, KeyAction::Quit);
    }

    #[test]
    fn ctrl_t_then_q_quits() {
        let mut state = EscapeState::Normal;
        let action1 = handle_key(
            make_key(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_eq!(action1, KeyAction::None);
        assert_eq!(state, EscapeState::CtrlT);

        let action2 = handle_key(
            make_key(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(action2, KeyAction::Quit);
        assert_eq!(state, EscapeState::Normal);
    }

    #[test]
    fn ctrl_t_ctrl_t_sends_literal() {
        let mut state = EscapeState::Normal;
        handle_key(
            make_key(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_eq!(state, EscapeState::CtrlT);

        let action = handle_key(
            make_key(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_eq!(action, KeyAction::SendByte(0x14));
        assert_eq!(state, EscapeState::Normal);
    }

    #[test]
    fn normal_key_forwards() {
        let mut state = EscapeState::Normal;
        let action = handle_key(
            make_key(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(action, KeyAction::SendByte(b'a'));
    }

    #[test]
    fn enter_sends_cr() {
        let mut state = EscapeState::Normal;
        let action = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(action, KeyAction::SendByte(b'\r'));
    }

    #[test]
    fn arrow_keys_send_escape_sequences() {
        let mut state = EscapeState::Normal;
        let action = handle_key(
            make_key(KeyCode::Up, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(action, KeyAction::SendBytes(b"\x1b[A".to_vec()));
    }

    #[test]
    fn ctrl_c_sends_control_code() {
        let mut state = EscapeState::Normal;
        let action = handle_key(
            make_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_eq!(action, KeyAction::SendByte(0x03)); // ETX
    }

    /// Helper: check that a string contains a timestamp pattern `[YYYY-MM-DD HH:MM:SS.mmm] `.
    fn has_timestamp(s: &str) -> bool {
        s.contains("] ") && s.contains("[20")
    }

    #[test]
    fn timestamper_prepends_on_first_line() {
        let mut ts = LineTimestamper::new();
        let out = ts.process(b"hello\r\n");
        let s = String::from_utf8(out).unwrap();
        assert!(has_timestamp(&s), "expected timestamp, got: {s}");
        assert!(s.contains("] hello\r\n"));
    }

    #[test]
    fn timestamper_each_line_gets_timestamp() {
        let mut ts = LineTimestamper::new();
        let out = ts.process(b"line1\nline2\nline3\n");
        let s = String::from_utf8(out).unwrap();
        // Each line should contain a timestamp
        for line in s.lines() {
            assert!(has_timestamp(line), "missing timestamp on: {line}");
        }
    }

    #[test]
    fn timestamper_partial_lines_across_calls() {
        let mut ts = LineTimestamper::new();
        // First chunk: partial line
        let out1 = ts.process(b"hel");
        let s1 = String::from_utf8(out1).unwrap();
        assert!(has_timestamp(&s1), "first chunk should have timestamp");
        assert!(s1.contains("hel"));

        // Second chunk: rest of line + newline + start of next
        let out2 = ts.process(b"lo\nworld");
        let s2 = String::from_utf8(out2).unwrap();
        assert!(s2.contains("lo\n"));
        assert!(s2.contains("] world"), "new line should get timestamp: {s2}");
    }

    #[test]
    fn timestamper_crlf_handled() {
        let mut ts = LineTimestamper::new();
        let out = ts.process(b"hello\r\nworld\r\n");
        let s = String::from_utf8(out).unwrap();
        assert!(has_timestamp(&s));
        let parts: Vec<&str> = s.split('\n').filter(|p| !p.is_empty()).collect();
        assert_eq!(parts.len(), 2);
        for part in &parts {
            assert!(has_timestamp(part), "missing timestamp on: {part}");
        }
    }

    #[test]
    fn timestamper_empty_input() {
        let mut ts = LineTimestamper::new();
        let out = ts.process(b"");
        assert!(out.is_empty());
    }

    #[test]
    fn timestamper_bare_newlines_no_spurious_timestamps() {
        let mut ts = LineTimestamper::new();
        let out = ts.process(b"\n\n");
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\n\n");
    }

    #[test]
    fn timestamper_preserves_color_across_lines() {
        let mut ts = LineTimestamper::new();
        // Line 1 sets green
        ts.process(b"\x1b[32mgreen text\r\n");
        // Line 2 should restore green after the timestamp
        let out = ts.process(b"still green\r\n");
        let s = String::from_utf8(out).unwrap();
        // Should have: reset + timestamp + green restore + "still green"
        assert!(s.contains("\x1b[0m"), "should reset before timestamp");
        assert!(s.contains("\x1b[32m"), "should restore green after timestamp");
        // Green restore should come after the timestamp
        let ts_end = s.find("] ").unwrap();
        let green_pos = s.find("\x1b[32m").unwrap();
        assert!(green_pos > ts_end, "green should come after timestamp");
    }

    #[test]
    fn timestamper_reset_clears_color_state() {
        let mut ts = LineTimestamper::new();
        // Set green then reset it
        ts.process(b"\x1b[32mgreen\x1b[0m\n");
        // Next line should NOT restore green
        let out = ts.process(b"no color\n");
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("\x1b[32m"), "green should not be restored after reset");
    }

    #[test]
    fn timestamper_multiple_sgr_attributes_preserved() {
        let mut ts = LineTimestamper::new();
        // Set bold + red
        ts.process(b"\x1b[1m\x1b[31mbold red\n");
        let out = ts.process(b"still styled\n");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[1m"), "should restore bold");
        assert!(s.contains("\x1b[31m"), "should restore red");
    }

    #[test]
    fn timestamper_combined_sgr_params() {
        let mut ts = LineTimestamper::new();
        // Combined bold+red in one sequence
        ts.process(b"\x1b[1;31mbold red\n");
        let out = ts.process(b"still styled\n");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[1;31m"), "should restore combined SGR");
    }

    #[test]
    fn timestamper_compound_reset_and_set() {
        let mut ts = LineTimestamper::new();
        // Set green first
        ts.process(b"\x1b[32mgreen\n");
        // Reset + set red in one sequence
        ts.process(b"\x1b[0;31mred\n");
        let out = ts.process(b"next line\n");
        let s = String::from_utf8(out).unwrap();
        // Should only restore red, not green
        assert!(!s.contains("\x1b[32m"), "green should have been cleared by reset");
        assert!(s.contains("\x1b[31m"), "should restore red");
    }

    #[test]
    fn timestamper_escape_sequence_split_across_calls() {
        let mut ts = LineTimestamper::new();
        // ESC arrives at end of one chunk, rest of CSI in next
        ts.process(b"\x1b");
        ts.process(b"[32m green text\n");
        let out = ts.process(b"next\n");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[32m"), "should restore green from split sequence");
    }

    #[test]
    fn timestamper_non_sgr_csi_ignored() {
        let mut ts = LineTimestamper::new();
        // Set green, then emit a cursor movement (CSI H), then newline
        ts.process(b"\x1b[32mgreen\x1b[H\n");
        let out = ts.process(b"next\n");
        let s = String::from_utf8(out).unwrap();
        // Green should still be tracked, cursor movement should not affect SGR state
        assert!(s.contains("\x1b[32m"), "non-SGR CSI should not affect color state");
    }

    #[test]
    fn timestamper_redundant_colors_replaced_by_reset_set() {
        let mut ts = LineTimestamper::new();
        // Accumulate several color changes without reset
        ts.process(b"\x1b[31m\x1b[32m\x1b[33m\x1b[34myellow then blue\n");
        // Now reset+set via compound param
        ts.process(b"\x1b[0;35mpurple\n");
        let out = ts.process(b"next\n");
        let s = String::from_utf8(out).unwrap();
        // Only purple should remain, all prior colors cleared
        assert!(!s.contains("\x1b[31m"));
        assert!(!s.contains("\x1b[32m"));
        assert!(!s.contains("\x1b[33m"));
        assert!(!s.contains("\x1b[34m"));
        assert!(s.contains("\x1b[35m"), "should restore purple");
    }

    #[test]
    fn timestamper_sgr_cap_prevents_unbounded_growth() {
        let mut ts = LineTimestamper::new();
        // Emit many color changes without any reset to exceed the 256-byte cap
        for i in 0..200 {
            let seq = format!("\x1b[38;5;{}m", i);
            ts.process(seq.as_bytes());
        }
        ts.process(b"text\n");
        let out = ts.process(b"next\n");
        let s = String::from_utf8(out).unwrap();
        // The restored SGR between timestamp and "next" should be bounded
        let ts_end = s.find("] ").unwrap();
        let next_pos = s.find("next").unwrap();
        let between = &s[ts_end + 2..next_pos];
        assert!(
            between.len() <= 300,
            "SGR replay should be bounded, got {} bytes",
            between.len()
        );
    }
}
