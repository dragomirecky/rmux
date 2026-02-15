/// The kind of message currently being accumulated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessageKind {
    /// Control message (client → server): `\0C{json}\0`
    Control,
    /// Response message (server → client): `\0R{json}\0`
    Response,
}

/// Events emitted by the parser as it processes bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseEvent {
    /// A regular data byte to pass through.
    DataByte(u8),
    /// A malformed escape: emit the NUL and the unexpected byte as raw data.
    MalformedEscape(u8),
    /// A complete structured message was parsed.
    Message { kind: MessageKind, json: String },
    /// No output yet (still accumulating).
    Pending,
}

/// Parser state machine states.
#[derive(Debug, Clone, PartialEq)]
enum ParserState {
    /// Normal data passthrough.
    Normal,
    /// Saw a NUL byte, waiting for the next byte to determine action.
    Escape,
    /// Inside a message, accumulating JSON bytes until the closing NUL.
    InMessage { kind: MessageKind, buf: Vec<u8> },
}

/// Maximum size (in bytes) of a single protocol message body.
/// Messages exceeding this are discarded to prevent memory exhaustion.
const MAX_MESSAGE_SIZE: usize = 1024 * 1024; // 1 MB

/// Stateful byte-by-byte protocol parser.
///
/// Protocol framing:
/// - `\0\0` → literal NUL byte in data stream
/// - `\0C{json}\0` → control message
/// - `\0R{json}\0` → response message
/// - Any other `\0X` → malformed escape (emit both bytes as data)
///
/// This parser is pure computation — no I/O, no async.
pub struct Parser {
    state: ParserState,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: ParserState::Normal,
        }
    }

    /// Feed a single byte into the parser and get the resulting event.
    pub fn feed(&mut self, byte: u8) -> ParseEvent {
        match std::mem::replace(&mut self.state, ParserState::Normal) {
            ParserState::Normal => {
                if byte == 0x00 {
                    self.state = ParserState::Escape;
                    ParseEvent::Pending
                } else {
                    ParseEvent::DataByte(byte)
                }
            }
            ParserState::Escape => match byte {
                0x00 => {
                    // Escaped NUL: \0\0 → literal NUL byte
                    ParseEvent::DataByte(0x00)
                }
                b'C' => {
                    self.state = ParserState::InMessage {
                        kind: MessageKind::Control,
                        buf: Vec::new(),
                    };
                    ParseEvent::Pending
                }
                b'R' => {
                    self.state = ParserState::InMessage {
                        kind: MessageKind::Response,
                        buf: Vec::new(),
                    };
                    ParseEvent::Pending
                }
                _ => {
                    // Malformed escape: not a recognized prefix
                    ParseEvent::MalformedEscape(byte)
                }
            },
            ParserState::InMessage { kind, mut buf } => {
                if byte == 0x00 {
                    // End of message
                    let json = String::from_utf8_lossy(&buf).into_owned();
                    ParseEvent::Message { kind, json }
                } else if buf.len() >= MAX_MESSAGE_SIZE {
                    // Message too large — stay in InMessage but stop growing the buffer
                    self.state = ParserState::InMessage { kind, buf };
                    ParseEvent::Pending
                } else {
                    buf.push(byte);
                    self.state = ParserState::InMessage { kind, buf };
                    ParseEvent::Pending
                }
            }
        }
    }

    /// Feed a slice of bytes and collect all non-Pending events.
    pub fn feed_bytes(&mut self, data: &[u8]) -> Vec<ParseEvent> {
        let mut events = Vec::new();
        for &byte in data {
            let event = self.feed(byte);
            if event != ParseEvent::Pending {
                events.push(event);
            }
        }
        events
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_data_passthrough() {
        let mut parser = Parser::new();
        let events = parser.feed_bytes(b"hello");
        assert_eq!(
            events,
            vec![
                ParseEvent::DataByte(b'h'),
                ParseEvent::DataByte(b'e'),
                ParseEvent::DataByte(b'l'),
                ParseEvent::DataByte(b'l'),
                ParseEvent::DataByte(b'o'),
            ]
        );
    }

    #[test]
    fn escaped_nul() {
        let mut parser = Parser::new();
        let events = parser.feed_bytes(&[0x00, 0x00]);
        assert_eq!(events, vec![ParseEvent::DataByte(0x00)]);
    }

    #[test]
    fn control_message() {
        let mut parser = Parser::new();
        let mut data = vec![0x00, b'C'];
        data.extend_from_slice(b"{\"cmd\":\"status\"}");
        data.push(0x00);

        let events = parser.feed_bytes(&data);
        assert_eq!(
            events,
            vec![ParseEvent::Message {
                kind: MessageKind::Control,
                json: "{\"cmd\":\"status\"}".to_string(),
            }]
        );
    }

    #[test]
    fn response_message() {
        let mut parser = Parser::new();
        let mut data = vec![0x00, b'R'];
        data.extend_from_slice(b"{\"status\":\"ok\"}");
        data.push(0x00);

        let events = parser.feed_bytes(&data);
        assert_eq!(
            events,
            vec![ParseEvent::Message {
                kind: MessageKind::Response,
                json: "{\"status\":\"ok\"}".to_string(),
            }]
        );
    }

    #[test]
    fn malformed_escape() {
        let mut parser = Parser::new();
        let events = parser.feed_bytes(&[0x00, b'X']);
        assert_eq!(events, vec![ParseEvent::MalformedEscape(b'X')]);
    }

    #[test]
    fn interleaved_data_and_control() {
        let mut parser = Parser::new();
        let mut data = Vec::new();
        data.extend_from_slice(b"before");
        data.push(0x00);
        data.push(b'C');
        data.extend_from_slice(b"{\"cmd\":\"status\"}");
        data.push(0x00);
        data.extend_from_slice(b"after");

        let events = parser.feed_bytes(&data);
        assert_eq!(events.len(), 6 + 1 + 5); // "before"(6) + message + "after"(5)
        // Check the message is in the middle
        assert_eq!(
            events[6],
            ParseEvent::Message {
                kind: MessageKind::Control,
                json: "{\"cmd\":\"status\"}".to_string(),
            }
        );
        // Check data before
        assert_eq!(events[0], ParseEvent::DataByte(b'b'));
        assert_eq!(events[5], ParseEvent::DataByte(b'e'));
        // Check data after
        assert_eq!(events[7], ParseEvent::DataByte(b'a'));
    }

    #[test]
    fn empty_json_message() {
        let mut parser = Parser::new();
        let data = vec![0x00, b'C', 0x00]; // empty JSON body
        let events = parser.feed_bytes(&data);
        assert_eq!(
            events,
            vec![ParseEvent::Message {
                kind: MessageKind::Control,
                json: String::new(),
            }]
        );
    }

    #[test]
    fn partial_message_then_complete() {
        let mut parser = Parser::new();

        // First feed: start of message
        let events1 = parser.feed_bytes(&[0x00, b'C', b'{']);
        assert!(events1.is_empty()); // all Pending

        // Second feed: rest of message
        let events2 = parser.feed_bytes(b"\"cmd\":\"status\"}");
        assert!(events2.is_empty()); // still accumulating

        // Third feed: terminating NUL
        let events3 = parser.feed_bytes(&[0x00]);
        assert_eq!(
            events3,
            vec![ParseEvent::Message {
                kind: MessageKind::Control,
                json: "{\"cmd\":\"status\"}".to_string(),
            }]
        );
    }

    #[test]
    fn very_long_json() {
        let mut parser = Parser::new();
        let long_value = "x".repeat(10000);
        let json = format!("{{\"data\":\"{long_value}\"}}");

        let mut data = vec![0x00, b'R'];
        data.extend_from_slice(json.as_bytes());
        data.push(0x00);

        let events = parser.feed_bytes(&data);
        assert_eq!(events.len(), 1);
        if let ParseEvent::Message { kind, json: parsed } = &events[0] {
            assert_eq!(*kind, MessageKind::Response);
            assert_eq!(*parsed, json);
        } else {
            panic!("expected Message event");
        }
    }

    #[test]
    fn multiple_escaped_nuls_in_data() {
        let mut parser = Parser::new();
        // Data: \0\0 A \0\0 → should produce NUL, 'A', NUL
        let data = vec![0x00, 0x00, b'A', 0x00, 0x00];
        let events = parser.feed_bytes(&data);
        assert_eq!(
            events,
            vec![
                ParseEvent::DataByte(0x00),
                ParseEvent::DataByte(b'A'),
                ParseEvent::DataByte(0x00),
            ]
        );
    }

    #[test]
    fn consecutive_messages() {
        let mut parser = Parser::new();
        let mut data = Vec::new();
        // First message
        data.push(0x00);
        data.push(b'C');
        data.extend_from_slice(b"{\"cmd\":\"status\"}");
        data.push(0x00);
        // Second message
        data.push(0x00);
        data.push(b'R');
        data.extend_from_slice(b"{\"status\":\"ok\"}");
        data.push(0x00);

        let events = parser.feed_bytes(&data);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            ParseEvent::Message {
                kind: MessageKind::Control,
                json: "{\"cmd\":\"status\"}".to_string(),
            }
        );
        assert_eq!(
            events[1],
            ParseEvent::Message {
                kind: MessageKind::Response,
                json: "{\"status\":\"ok\"}".to_string(),
            }
        );
    }

    #[test]
    fn oversized_message_buffer_capped() {
        let mut parser = Parser::new();
        // Start a control message
        let mut data = vec![0x00, b'C'];
        // Feed MAX_MESSAGE_SIZE + 100 bytes of content
        data.extend(vec![b'x'; super::MAX_MESSAGE_SIZE + 100]);
        // Close the message
        data.push(0x00);

        let events = parser.feed_bytes(&data);
        // Should emit exactly one Message event with the body capped at MAX_MESSAGE_SIZE
        assert_eq!(events.len(), 1);
        if let ParseEvent::Message { kind, json } = &events[0] {
            assert_eq!(*kind, MessageKind::Control);
            assert_eq!(json.len(), super::MAX_MESSAGE_SIZE);
        } else {
            panic!("expected Message event, got {:?}", events[0]);
        }

        // Parser should recover and handle normal data after
        let events2 = parser.feed_bytes(b"ok");
        assert_eq!(
            events2,
            vec![ParseEvent::DataByte(b'o'), ParseEvent::DataByte(b'k')]
        );
    }

    #[test]
    fn data_byte_by_byte() {
        let mut parser = Parser::new();
        assert_eq!(parser.feed(b'A'), ParseEvent::DataByte(b'A'));
        assert_eq!(parser.feed(0x00), ParseEvent::Pending);
        assert_eq!(parser.feed(0x00), ParseEvent::DataByte(0x00));
        assert_eq!(parser.feed(b'B'), ParseEvent::DataByte(b'B'));
    }
}
