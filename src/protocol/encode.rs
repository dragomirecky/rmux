use super::message::{ControlMessage, ResponseMessage};
use super::parser::MessageKind;

/// Encode raw data bytes, escaping any NUL bytes as `\0\0`.
pub fn encode_data(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &byte in data {
        if byte == 0x00 {
            out.push(0x00);
            out.push(0x00);
        } else {
            out.push(byte);
        }
    }
    out
}

/// Encode a structured message as a framed byte sequence: `\0{prefix}{json}\0`
fn encode_message(kind: MessageKind, json: &str) -> Vec<u8> {
    let prefix = match kind {
        MessageKind::Control => b'C',
        MessageKind::Response => b'R',
    };
    let mut out = Vec::with_capacity(json.len() + 3);
    out.push(0x00);
    out.push(prefix);
    out.extend_from_slice(json.as_bytes());
    out.push(0x00);
    out
}

/// Encode a control message into its wire format.
pub fn encode_control(msg: &ControlMessage) -> Vec<u8> {
    let json = msg.to_json();
    encode_message(MessageKind::Control, &json)
}

/// Encode a response message into its wire format.
pub fn encode_response(msg: &ResponseMessage) -> Vec<u8> {
    let json = serde_json::to_string(msg).expect("ResponseMessage serialization should not fail");
    encode_message(MessageKind::Response, &json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::message::*;
    use crate::protocol::parser::{ParseEvent, Parser};

    #[test]
    fn encode_data_no_nuls() {
        let data = b"hello world";
        assert_eq!(encode_data(data), data.to_vec());
    }

    #[test]
    fn encode_data_with_nuls() {
        let data = vec![b'A', 0x00, b'B'];
        let encoded = encode_data(&data);
        assert_eq!(encoded, vec![b'A', 0x00, 0x00, b'B']);
    }

    #[test]
    fn encode_data_all_nuls() {
        let data = vec![0x00, 0x00, 0x00];
        let encoded = encode_data(&data);
        assert_eq!(encoded, vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn encode_control_status() {
        let encoded = encode_control(&ControlMessage::Status);
        assert_eq!(encoded[0], 0x00);
        assert_eq!(encoded[1], b'C');
        assert_eq!(*encoded.last().unwrap(), 0x00);
        let json = std::str::from_utf8(&encoded[2..encoded.len() - 1]).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["cmd"], "status");
    }

    #[test]
    fn encode_control_history() {
        let msg = ControlMessage::History {
            lines: 100,
            timestamps: true,
            since: None,
        };
        let encoded = encode_control(&msg);
        assert_eq!(encoded[0], 0x00);
        assert_eq!(encoded[1], b'C');
        assert_eq!(*encoded.last().unwrap(), 0x00);
    }

    #[test]
    fn encode_response_status() {
        let resp = ResponseMessage::Status(StatusResponse {
            status: "ok".into(),
            serial: true,
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
        });
        let encoded = encode_response(&resp);
        assert_eq!(encoded[0], 0x00);
        assert_eq!(encoded[1], b'R');
        assert_eq!(*encoded.last().unwrap(), 0x00);
    }

    #[test]
    fn encode_response_event() {
        let resp = ResponseMessage::Event(EventResponse {
            event: "serial_connected".into(),
        });
        let encoded = encode_response(&resp);
        let json = std::str::from_utf8(&encoded[2..encoded.len() - 1]).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["event"], "serial_connected");
    }

    #[test]
    fn encode_response_error() {
        let resp = ResponseMessage::Error(ErrorResponse {
            error: "unknown command".into(),
        });
        let encoded = encode_response(&resp);
        let json = std::str::from_utf8(&encoded[2..encoded.len() - 1]).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["error"], "unknown command");
    }

    // Round-trip tests: encode → parse → verify
    #[test]
    fn round_trip_data_no_nuls() {
        let data = b"hello world";
        let encoded = encode_data(data);
        let mut parser = Parser::new();
        let events = parser.feed_bytes(&encoded);
        let bytes: Vec<u8> = events
            .into_iter()
            .map(|e| match e {
                ParseEvent::DataByte(b) => b,
                _ => panic!("expected DataByte"),
            })
            .collect();
        assert_eq!(bytes, data.to_vec());
    }

    #[test]
    fn round_trip_data_with_nuls() {
        let data = vec![b'A', 0x00, b'B', 0x00, 0x00, b'C'];
        let encoded = encode_data(&data);
        let mut parser = Parser::new();
        let events = parser.feed_bytes(&encoded);
        let bytes: Vec<u8> = events
            .into_iter()
            .map(|e| match e {
                ParseEvent::DataByte(b) => b,
                _ => panic!("expected DataByte"),
            })
            .collect();
        assert_eq!(bytes, data);
    }

    #[test]
    fn round_trip_control_message() {
        let msg = ControlMessage::History {
            lines: 50,
            timestamps: false,
            since: None,
        };
        let encoded = encode_control(&msg);
        let mut parser = Parser::new();
        let events = parser.feed_bytes(&encoded);
        assert_eq!(events.len(), 1);
        if let ParseEvent::Message {
            kind: MessageKind::Control,
            json,
        } = &events[0]
        {
            let parsed = ControlMessage::from_json(json);
            assert_eq!(parsed, msg);
        } else {
            panic!("expected Control message event");
        }
    }

    #[test]
    fn round_trip_response_message() {
        let resp = ResponseMessage::Status(StatusResponse {
            status: "ok".into(),
            serial: true,
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
        });
        let encoded = encode_response(&resp);
        let mut parser = Parser::new();
        let events = parser.feed_bytes(&encoded);
        assert_eq!(events.len(), 1);
        if let ParseEvent::Message {
            kind: MessageKind::Response,
            json,
        } = &events[0]
        {
            let parsed: StatusResponse = serde_json::from_str(json).unwrap();
            assert_eq!(parsed.status, "ok");
            assert_eq!(parsed.baudrate, 115_200);
        } else {
            panic!("expected Response message event");
        }
    }

    #[test]
    fn round_trip_mixed_data_and_messages() {
        let mut wire = Vec::new();
        // Some data
        wire.extend_from_slice(&encode_data(b"hello"));
        // A control message
        wire.extend_from_slice(&encode_control(&ControlMessage::Status));
        // More data with a NUL
        wire.extend_from_slice(&encode_data(&[0x00, b'!']));

        let mut parser = Parser::new();
        let events = parser.feed_bytes(&wire);

        // 5 data bytes + 1 control message + 2 data bytes = 8 events
        assert_eq!(events.len(), 8);
        assert_eq!(events[0], ParseEvent::DataByte(b'h'));
        assert!(matches!(
            &events[5],
            ParseEvent::Message {
                kind: MessageKind::Control,
                ..
            }
        ));
        assert_eq!(events[6], ParseEvent::DataByte(0x00));
        assert_eq!(events[7], ParseEvent::DataByte(b'!'));
    }
}
