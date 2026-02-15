use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::protocol::{encode_data, MessageKind, ParseEvent, Parser};

/// Run the client in interactive mode with raw terminal I/O.
///
/// Keyboard input is forwarded to the server. Serial data received from
/// the server is displayed on the terminal. Ctrl+] quits.
pub async fn run_interactive(
    stream: UnixStream,
    timestamps: bool,
) -> anyhow::Result<()> {
    let (mut read_half, mut write_half) = stream.into_split();

    // Enable raw mode
    crossterm::terminal::enable_raw_mode()?;

    let result = run_interactive_inner(&mut read_half, &mut write_half, timestamps).await;

    // Always restore terminal mode
    crossterm::terminal::disable_raw_mode()?;
    println!(); // newline after raw mode

    result
}

async fn run_interactive_inner(
    read_half: &mut tokio::net::unix::OwnedReadHalf,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    _timestamps: bool,
) -> anyhow::Result<()> {
    use crossterm::event::{Event, EventStream};
    use futures::StreamExt;

    let mut event_stream = EventStream::new();
    let mut parser = Parser::new();
    let mut buf = [0u8; 4096];
    let mut escape_state = EscapeState::Normal;

    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            // Read from server
            result = read_half.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        eprintln!("\r\nConnection closed by server");
                        break;
                    }
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
                                ParseEvent::Message { kind: MessageKind::Response, json } => {
                                    // Display response messages
                                    let msg = format!("\r\n[server: {json}]\r\n");
                                    stdout.write_all(msg.as_bytes()).await?;
                                }
                                _ => {}
                            }
                        }
                        if !data_buf.is_empty() {
                            stdout.write_all(&data_buf).await?;
                            stdout.flush().await?;
                        }
                    }
                    Err(e) => {
                        eprintln!("\r\nRead error: {e}");
                        break;
                    }
                }
            }
            // Read keyboard input
            event = event_stream.next() => {
                match event {
                    Some(Ok(Event::Key(key_event))) => {
                        match handle_key(key_event, &mut escape_state) {
                            KeyAction::Quit => break,
                            KeyAction::SendByte(b) => {
                                let encoded = encode_data(&[b]);
                                write_half.write_all(&encoded).await?;
                            }
                            KeyAction::SendBytes(bytes) => {
                                let encoded = encode_data(&bytes);
                                write_half.write_all(&encoded).await?;
                            }
                            KeyAction::None => {}
                        }
                    }
                    Some(Ok(_)) => {} // ignore non-key events
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
}
