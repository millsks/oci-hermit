//! Control protocol between the oci-hermit host tool and the guest agent.
//!
//! Messages travel over a virtio-serial channel as newline-delimited JSON.
//! This is a placeholder shape for milestone M1; the framing and message set
//! will be settled in the control-protocol issue.

use serde::{Deserialize, Serialize};

/// Protocol version. Host and agent refuse to talk if the major versions differ.
pub const PROTOCOL_VERSION: (u16, u16) = (0, 1);

/// Messages sent from the host tool to the guest agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// First message on a new channel.
    Hello { version: (u16, u16) },
    /// Ask the agent to power off the guest.
    Shutdown,
}

/// Messages sent from the guest agent to the host tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestMessage {
    /// Reply to `Hello`.
    Ready { version: (u16, u16) },
    /// A request failed.
    Error { message: String },
}

/// Encode a message as one line of JSON, including the trailing newline.
pub fn encode<T: Serialize>(msg: &T) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Decode one line of JSON into a message.
pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let msg = HostMessage::Hello {
            version: PROTOCOL_VERSION,
        };
        let line = encode(&msg).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(decode::<HostMessage>(&line).unwrap(), msg);
    }

    #[test]
    fn messages_are_tagged() {
        let line = encode(&GuestMessage::Error {
            message: "nope".into(),
        })
        .unwrap();
        assert!(line.contains("\"type\":\"error\""));
    }
}
