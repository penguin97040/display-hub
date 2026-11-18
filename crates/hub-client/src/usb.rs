//! Sending a provisioning bundle to a display over its USB serial port.
//!
//! The caller opens the port (115200 baud, with a short read timeout) and passes it in, so this works
//! with any `Read + Write`: a real serial port, or a pseudo-terminal in tests. It blocks, so run it on
//! a blocking thread.
//!
//! The framing is in `docs/protocol-v1.md`, "Over USB"; `display-simulator usb` is the reference
//! display for it. Real firmware speaking it hasn't been tried yet.

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use hub_protocol::device::DeviceInfo;
use hub_protocol::provisioning::Bundle;
use hub_protocol::usb::{
    encode, FromDisplay, Line, LineReader, ToDisplay, UsbErrorCode, ANSWER_TIMEOUT_SECONDS,
};
use hub_protocol::PROTOCOL_VERSION;

/// The display that answered.
#[derive(Clone, Debug, PartialEq)]
pub struct UsbDisplay {
    pub device: DeviceInfo,
    pub provisioned: bool,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum UsbError {
    #[error("the serial port failed: {0}")]
    Port(String),
    #[error("nothing answered on this port. Check it's the display, that it's switched on, and try again.")]
    NoAnswer,
    #[error("the display answered something this app doesn't understand")]
    Unexpected,
    #[error("the display speaks protocol version {0}, which this app doesn't")]
    Incompatible(u32),
    #[error(
        "the display is already set up. To point it at this hub, factory-reset it on the device itself, then try again."
    )]
    AlreadyProvisioned,
    #[error("the display refused the setup details: {0}")]
    InvalidBundle(String),
    #[error("the display couldn't save the setup details: {0}")]
    StorageFailed(String),
    #[error(
        "the display already trusts firmware from a different maker's keys, so it refused these setup \
         details. Only Install firmware, which erases the display, can change whose firmware it accepts."
    )]
    FirmwareKeysLocked,
    #[error("the display refused the setup details ({0})")]
    Refused(String),
}

/// How often `hello` is repeated while nothing answers. A board that resets when the port opens
/// misses the first one while it boots.
const HELLO_REPEAT: Duration = Duration::from_secs(2);

struct Conn<'a, P: Read + Write> {
    port: &'a mut P,
    lines: LineReader,
    /// Lines read but not yet handled: one read can bring several.
    pending: std::collections::VecDeque<Line>,
    /// Whether the display has answered `hello`: after that, a write that stalls is a port problem.
    answered: bool,
}

impl<P: Read + Write> Conn<'_, P> {
    fn send(&mut self, msg: &ToDisplay) -> Result<(), UsbError> {
        let answered = self.answered;
        self.port
            .write_all(&encode(msg))
            .map_err(|e| write_error(e, answered))?;
        self.port.flush().map_err(|e| write_error(e, answered))
    }

    /// The next JSON message, or `None` at the deadline. `repeat` is resent every [`HELLO_REPEAT`].
    fn next(
        &mut self,
        deadline: Instant,
        repeat: Option<&ToDisplay>,
    ) -> Result<Option<FromDisplay>, UsbError> {
        let mut last_sent = Instant::now();
        let mut buf = [0u8; 1024];
        loop {
            if let Some(line) = self.pending.pop_front() {
                match line {
                    Line::Json(s) => {
                        return Ok(Some(serde_json::from_str(&s).map_err(|_| UsbError::Unexpected)?))
                    }
                    Line::Overlong => return Err(UsbError::Unexpected),
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            if let Some(msg) = repeat {
                if last_sent.elapsed() >= HELLO_REPEAT {
                    self.send(msg)?;
                    last_sent = Instant::now();
                }
            }
            match self.port.read(&mut buf) {
                Ok(0) => std::thread::sleep(Duration::from_millis(20)),
                Ok(n) => {
                    let lines = self.lines.push(&buf[..n]);
                    self.pending.extend(lines)
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(port_error(e)),
            }
        }
    }
}

fn port_error(e: std::io::Error) -> UsbError {
    UsbError::Port(e.to_string())
}

/// Before anything has answered, a write that times out (Windows says "os error 121") or writes
/// nothing (`WriteZero`) means nothing on the other end is reading the port: a board whose firmware
/// doesn't speak this exchange, not a port in use. Once the display has answered, it is a port error.
fn write_error(e: std::io::Error, answered: bool) -> UsbError {
    match e.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::WriteZero if !answered => UsbError::NoAnswer,
        _ => port_error(e),
    }
}

fn answer_deadline() -> Instant {
    Instant::now() + Duration::from_secs(ANSWER_TIMEOUT_SECONDS)
}

/// Asks what is on the port.
pub fn hello<P: Read + Write>(port: &mut P) -> Result<UsbDisplay, UsbError> {
    let mut conn = Conn {
        port,
        lines: LineReader::default(),
        pending: Default::default(),
        answered: false,
    };
    hello_on(&mut conn)
}

fn hello_on<P: Read + Write>(conn: &mut Conn<'_, P>) -> Result<UsbDisplay, UsbError> {
    let msg = ToDisplay::Hello {
        protocol_version: PROTOCOL_VERSION,
    };
    conn.send(&msg)?;
    let deadline = answer_deadline();
    loop {
        match conn.next(deadline, Some(&msg))? {
            None => return Err(UsbError::NoAnswer),
            Some(FromDisplay::Hello { device, provisioned }) => {
                if device.protocol_version != PROTOCOL_VERSION {
                    return Err(UsbError::Incompatible(device.protocol_version));
                }
                conn.answered = true;
                return Ok(UsbDisplay { device, provisioned });
            }
            // A late answer to an earlier session. Keep waiting for the hello.
            Some(_) => continue,
        }
    }
}

/// Says hello, then sends the bundle. Returns the display that accepted it.
pub fn provision<P: Read + Write>(port: &mut P, bundle: &Bundle) -> Result<UsbDisplay, UsbError> {
    let mut conn = Conn {
        port,
        lines: LineReader::default(),
        pending: Default::default(),
        answered: false,
    };
    let display = hello_on(&mut conn)?;
    if display.provisioned {
        return Err(UsbError::AlreadyProvisioned);
    }
    conn.send(&ToDisplay::Provision {
        bundle: bundle.clone(),
    })?;
    let deadline = answer_deadline();
    loop {
        match conn.next(deadline, None)? {
            None => return Err(UsbError::NoAnswer),
            Some(FromDisplay::Provisioned) => return Ok(display),
            Some(FromDisplay::Error { code, message }) => {
                let message: String = message.chars().filter(|c| !c.is_control()).take(200).collect();
                return Err(match code {
                    UsbErrorCode::AlreadyProvisioned => UsbError::AlreadyProvisioned,
                    UsbErrorCode::InvalidBundle => UsbError::InvalidBundle(message),
                    UsbErrorCode::StorageFailed => UsbError::StorageFailed(message),
                    UsbErrorCode::FirmwareKeysLocked => UsbError::FirmwareKeysLocked,
                    UsbErrorCode::Unrecognised => UsbError::Refused(message),
                });
            }
            // Repeated hellos may be answered more than once.
            Some(FromDisplay::Hello { .. }) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A port that hands back scripted chunks, one per read.
    struct Scripted {
        reads: VecDeque<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.reads.pop_front() {
                Some(c) => {
                    buf[..c.len()].copy_from_slice(&c);
                    Ok(c.len())
                }
                None => Err(std::io::ErrorKind::TimedOut.into()),
            }
        }
    }

    impl Write for Scripted {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A port nothing reads: every write times out, as Windows does when the board ignores it.
    struct Unread;

    impl Read for Unread {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::TimedOut.into())
        }
    }

    impl Write for Unread {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::TimedOut.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_board_that_never_reads_is_no_answer_not_a_busy_port() {
        let bundle: Bundle = serde_json::from_str(include_str!(
            "../../../protocol/v1/examples/device/provisioning-bundle.json"
        ))
        .unwrap();
        assert_eq!(provision(&mut Unread, &bundle).unwrap_err(), UsbError::NoAnswer);
    }

    /// Answers `hello`, then stalls (or writes nothing) on the next write.
    struct StallsAfterHello {
        inner: Scripted,
        hello_done: bool,
        kind: std::io::ErrorKind,
    }

    impl Read for StallsAfterHello {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl Write for StallsAfterHello {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if !self.hello_done {
                self.hello_done = true;
                return self.inner.write(b);
            }
            match self.kind {
                std::io::ErrorKind::WriteZero => Ok(0),
                k => Err(k.into()),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_write_that_stalls_after_the_display_answered_is_a_port_error() {
        let bundle: Bundle = serde_json::from_str(include_str!(
            "../../../protocol/v1/examples/device/provisioning-bundle.json"
        ))
        .unwrap();
        for kind in [std::io::ErrorKind::TimedOut, std::io::ErrorKind::WriteZero] {
            let hello = "{\"type\":\"hello\",\"device_type\":\"d\",\"firmware_version\":\"1\",\"hardware_revision\":\"r\",\"protocol_version\":1,\"provisioned\":false}\n";
            let mut port = StallsAfterHello {
                inner: Scripted {
                    reads: VecDeque::from([hello.as_bytes().to_vec()]),
                    written: Vec::new(),
                },
                hello_done: false,
                kind,
            };
            let err = provision(&mut port, &bundle).unwrap_err();
            assert!(matches!(err, UsbError::Port(_)), "{kind:?}: {err:?}");
        }
    }

    #[test]
    fn a_write_of_nothing_before_any_answer_is_no_answer() {
        assert_eq!(
            write_error(std::io::ErrorKind::WriteZero.into(), false),
            UsbError::NoAnswer
        );
    }

    #[test]
    fn several_answers_in_one_read_are_all_handled() {
        let bundle: Bundle = serde_json::from_str(include_str!(
            "../../../protocol/v1/examples/device/provisioning-bundle.json"
        ))
        .unwrap();
        let hello = r#"{"type":"hello","device_type":"d","firmware_version":"1","hardware_revision":"r","protocol_version":1,"provisioned":false}"#;
        // A repeated hello answered twice, then "provisioned", all in one chunk.
        let chunk = format!("{hello}\n{hello}\n{{\"type\":\"provisioned\"}}\n");
        let mut port = Scripted {
            reads: VecDeque::from([chunk.into_bytes()]),
            written: Vec::new(),
        };
        assert!(provision(&mut port, &bundle).is_ok());
    }
}
