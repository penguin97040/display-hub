//! Provisioning over USB serial: the messages and their line framing (`docs/protocol-v1.md`, "Over
//! USB").
//!
//! One UTF-8 JSON object per line, ending in `\n`, at most [`MAX_LINE_BYTES`]. Lines that don't start
//! with `{` are ignored by both sides, so boot logs on the same port are harmless.

use serde::{Deserialize, Serialize};

use crate::device::DeviceInfo;
use crate::provisioning::Bundle;

pub const BAUD_RATE: u32 = 115_200;
/// The longest line either side sends or accepts, `\n` included.
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// How long either side waits for each answer.
pub const ANSWER_TIMEOUT_SECONDS: u64 = 10;

/// What the manager sends.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToDisplay {
    Hello { protocol_version: u32 },
    Provision { bundle: Bundle },
}

impl std::fmt::Debug for ToDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToDisplay::Hello { protocol_version } => f
                .debug_struct("Hello")
                .field("protocol_version", protocol_version)
                .finish(),
            // Bundle's own Debug leaves out the secret and the Wi-Fi password.
            ToDisplay::Provision { bundle } => f.debug_struct("Provision").field("bundle", bundle).finish(),
        }
    }
}

/// What the display answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromDisplay {
    Hello {
        #[serde(flatten)]
        device: DeviceInfo,
        /// Whether it already holds a bundle. If so, it refuses another until factory-reset.
        provisioned: bool,
    },
    Provisioned,
    Error {
        code: UsbErrorCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbErrorCode {
    /// The bundle broke a rule, or isn't for this protocol version.
    InvalidBundle,
    /// The display is already set up. It must be factory-reset on the device itself first.
    AlreadyProvisioned,
    /// The display couldn't save the bundle.
    StorageFailed,
    /// A code from a newer revision.
    #[serde(other)]
    Unrecognised,
}

/// One message as a line, ready to write.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(msg).expect("protocol messages always serialise");
    line.push(b'\n');
    line
}

/// One framed line.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A line starting with `{`, without its line ending. Not yet parsed.
    Json(String),
    /// A line longer than [`MAX_LINE_BYTES`] was dropped.
    Overlong,
}

/// Splits a byte stream into lines, skipping anything that isn't JSON and dropping overlong lines
/// without ever holding more than [`MAX_LINE_BYTES`].
#[derive(Debug, Default)]
pub struct LineReader {
    buf: Vec<u8>,
    discarding: bool,
}

impl LineReader {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Line> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if self.discarding {
                    self.discarding = false;
                } else {
                    let mut line = std::mem::take(&mut self.buf);
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    if line.first() == Some(&b'{') {
                        // Invalid UTF-8 can't be JSON; keep it so the parser reports it.
                        out.push(Line::Json(String::from_utf8_lossy(&line).into_owned()));
                    }
                }
            } else if !self.discarding {
                self.buf.push(b);
                if self.buf.len() >= MAX_LINE_BYTES {
                    self.buf.clear();
                    self.discarding = true;
                    out.push(Line::Overlong);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_skip_logs_and_split_anywhere() {
        let mut r = LineReader::default();
        assert!(r
            .push(b"I (312) boot: ESP-IDF v5.3\r\n{\"type\":\"pro")
            .is_empty());
        let lines = r.push(b"visioned\"}\r\n\n  {\"not\":\"first\"}\n");
        assert_eq!(lines, vec![Line::Json(r#"{"type":"provisioned"}"#.into())]);
        let msg: FromDisplay = match &lines[0] {
            Line::Json(s) => serde_json::from_str(s).unwrap(),
            Line::Overlong => unreachable!(),
        };
        assert_eq!(msg, FromDisplay::Provisioned);
    }

    #[test]
    fn overlong_lines_are_dropped_and_the_next_one_is_read() {
        let mut r = LineReader::default();
        let mut lines = r.push(&vec![b'{'; MAX_LINE_BYTES + 100]);
        lines.extend(r.push(b"tail\n{\"type\":\"provisioned\"}\n"));
        assert_eq!(
            lines,
            vec![Line::Overlong, Line::Json(r#"{"type":"provisioned"}"#.into())]
        );
        assert!(r.buf.capacity() <= MAX_LINE_BYTES * 2);
    }

    #[test]
    fn messages_match_the_documented_shapes() {
        assert_eq!(
            encode(&ToDisplay::Hello { protocol_version: 1 }),
            b"{\"type\":\"hello\",\"protocol_version\":1}\n"
        );
        let hello: FromDisplay = serde_json::from_str(
            r#"{"type":"hello","device_type":"llm-usage-display","firmware_version":"1.0.0",
                "hardware_revision":"waveshare-amoled-1.8-v2","protocol_version":1,"provisioned":false,
                "extra":"ignored"}"#,
        )
        .unwrap();
        let FromDisplay::Hello { device, provisioned } = hello else {
            panic!("hello")
        };
        assert_eq!(
            (device.device_type.as_str(), provisioned),
            ("llm-usage-display", false)
        );
        let err: FromDisplay =
            serde_json::from_str(r#"{"type":"error","code":"from_the_future","message":"x"}"#).unwrap();
        assert_eq!(
            err,
            FromDisplay::Error {
                code: UsbErrorCode::Unrecognised,
                message: "x".into()
            }
        );
    }
}
