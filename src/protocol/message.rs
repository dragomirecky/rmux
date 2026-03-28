use serde::{Deserialize, Serialize};

/// Control messages sent from client to server.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlMessage {
    History {
        lines: usize,
        timestamps: bool,
        since: Option<String>,
    },
    Status,
    Disconnect,
    Unknown(String),
}

/// Response messages sent from server to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseMessage {
    Status(StatusResponse),
    Event(EventResponse),
    ServerInfo(ServerInfoResponse),
    Error(ErrorResponse),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub serial: bool,
    pub port: String,
    pub baudrate: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventResponse {
    pub event: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInfoResponse {
    pub serial: bool,
    pub port: String,
    pub baudrate: u32,
    pub clients: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

impl ControlMessage {
    /// Parse a control message from a JSON string.
    pub fn from_json(json: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            return ControlMessage::Unknown(json.to_string());
        };

        match value.get("cmd").and_then(|v| v.as_str()) {
            Some("history") => {
                let lines = value
                    .get("lines")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(50);
                let timestamps = value
                    .get("timestamps")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let since = value
                    .get("since")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);
                ControlMessage::History {
                    lines,
                    timestamps,
                    since,
                }
            }
            Some("status") => ControlMessage::Status,
            Some("disconnect") => ControlMessage::Disconnect,
            _ => ControlMessage::Unknown(json.to_string()),
        }
    }

    /// Serialize this control message to JSON.
    pub fn to_json(&self) -> String {
        match self {
            ControlMessage::History {
                lines,
                timestamps,
                since,
            } => {
                let mut obj = serde_json::json!({
                    "cmd": "history",
                    "lines": lines,
                    "timestamps": timestamps,
                });
                if let Some(ref s) = since {
                    obj["since"] = serde_json::json!(s);
                }
                obj.to_string()
            }
            ControlMessage::Status => {
                serde_json::json!({"cmd": "status"}).to_string()
            }
            ControlMessage::Disconnect => {
                serde_json::json!({"cmd": "disconnect"}).to_string()
            }
            ControlMessage::Unknown(s) => s.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_history_command() {
        let msg = ControlMessage::from_json(r#"{"cmd":"history","lines":50,"timestamps":false}"#);
        assert_eq!(
            msg,
            ControlMessage::History {
                lines: 50,
                timestamps: false,
                since: None,
            }
        );
    }

    #[test]
    fn parse_history_defaults() {
        let msg = ControlMessage::from_json(r#"{"cmd":"history"}"#);
        assert_eq!(
            msg,
            ControlMessage::History {
                lines: 50,
                timestamps: false,
                since: None,
            }
        );
    }

    #[test]
    fn parse_history_with_since() {
        let msg = ControlMessage::from_json(
            r#"{"cmd":"history","lines":50,"timestamps":false,"since":"U-Boot"}"#,
        );
        assert_eq!(
            msg,
            ControlMessage::History {
                lines: 50,
                timestamps: false,
                since: Some("U-Boot".into()),
            }
        );
    }

    #[test]
    fn history_since_round_trip() {
        let original = ControlMessage::History {
            lines: 50,
            timestamps: false,
            since: Some("BOOT".into()),
        };
        let json = original.to_json();
        let parsed = ControlMessage::from_json(&json);
        assert_eq!(original, parsed);
    }

    #[test]
    fn parse_status_command() {
        let msg = ControlMessage::from_json(r#"{"cmd":"status"}"#);
        assert_eq!(msg, ControlMessage::Status);
    }

    #[test]
    fn parse_disconnect_command() {
        let msg = ControlMessage::from_json(r#"{"cmd":"disconnect"}"#);
        assert_eq!(msg, ControlMessage::Disconnect);
    }

    #[test]
    fn parse_unknown_command() {
        let msg = ControlMessage::from_json(r#"{"cmd":"foobar"}"#);
        assert!(matches!(msg, ControlMessage::Unknown(_)));
    }

    #[test]
    fn parse_invalid_json() {
        let msg = ControlMessage::from_json("not json");
        assert!(matches!(msg, ControlMessage::Unknown(_)));
    }

    #[test]
    fn control_message_round_trip() {
        let original = ControlMessage::History {
            lines: 100,
            timestamps: true,
            since: None,
        };
        let json = original.to_json();
        let parsed = ControlMessage::from_json(&json);
        assert_eq!(original, parsed);
    }

    #[test]
    fn status_response_serialize() {
        let resp = StatusResponse {
            status: "ok".into(),
            serial: true,
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"serial\":true"));
    }

    #[test]
    fn event_response_serialize() {
        let resp = EventResponse {
            event: "history_end".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"event\":\"history_end\""));
    }

    #[test]
    fn error_response_serialize() {
        let resp = ErrorResponse {
            error: "unknown command".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"error\":\"unknown command\""));
    }

    #[test]
    fn server_info_response_serialize() {
        let resp = ServerInfoResponse {
            serial: true,
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            clients: 2,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["clients"], 2);
        assert_eq!(parsed["serial"], true);
    }

    #[test]
    fn status_round_trip() {
        let json = ControlMessage::Status.to_json();
        let parsed = ControlMessage::from_json(&json);
        assert_eq!(parsed, ControlMessage::Status);
    }

    #[test]
    fn disconnect_round_trip() {
        let json = ControlMessage::Disconnect.to_json();
        let parsed = ControlMessage::from_json(&json);
        assert_eq!(parsed, ControlMessage::Disconnect);
    }
}
