//! The real [`Board`]: an ESP32 in its built-in loader, through the esp-rs project's `espflash`
//! library. Everything this does is decided in `firmware.rs`; this only carries it out, and sorts
//! espflash's errors into the kinds a person can act on.

use std::time::Duration;

use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::Flasher;
use espflash::target::ProgressCallbacks;
use serialport::{SerialPortType, UsbPortInfo};

use crate::firmware::{Board, BoardError, BoardInfo};

/// The speed the loader starts at. It is kept for the whole install: slower than espflash can go,
/// but it works with every USB serial chip and cable.
const BAUD: u32 = 115_200;

pub struct EspBoard {
    port: String,
    flasher: Option<Flasher>,
}

impl EspBoard {
    pub fn new(port: &str) -> Self {
        EspBoard {
            port: port.to_string(),
            flasher: None,
        }
    }

    fn flasher(&mut self) -> Result<&mut Flasher, BoardError> {
        self.flasher
            .as_mut()
            .ok_or_else(|| BoardError::Other("not connected to the board".into()))
    }
}

/// The USB details of a port, which tell espflash how to reset the board (boards with the chip's
/// own USB reset differently from ones with a USB serial chip). Zeros when it isn't a USB port.
fn usb_info(port: &str) -> UsbPortInfo {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.port_name == port)
        .and_then(|p| match p.port_type {
            SerialPortType::UsbPort(u) => Some(u),
            _ => None,
        })
        .unwrap_or(UsbPortInfo {
            vid: 0,
            pid: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
        })
}

/// A port that wouldn't open.
pub fn open_error(port: &str, e: &serialport::Error) -> BoardError {
    let text = e.to_string();
    let lower = text.to_lowercase();
    match e.kind() {
        serialport::ErrorKind::NoDevice => BoardError::NoBoard(text),
        serialport::ErrorKind::Io(std::io::ErrorKind::NotFound) => BoardError::NoBoard(text),
        serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied) => BoardError::Busy(text),
        _ if lower.contains("denied") || lower.contains("busy") || lower.contains("in use") => {
            BoardError::Busy(text)
        }
        _ if lower.contains("not found") || lower.contains("no such") || lower.contains("cannot find") => {
            BoardError::NoBoard(format!("{port}: {text}"))
        }
        _ => BoardError::Other(text),
    }
}

/// An espflash error, sorted by what to do about it.
fn sort(e: espflash::Error) -> BoardError {
    use espflash::Error as E;
    let text = e.to_string();
    let detail = match std::error::Error::source(&e) {
        Some(s) => format!("{text}: {s}"),
        None => text.clone(),
    };
    let lower = detail.to_lowercase();
    match e {
        E::Connection(_) => {
            if lower.contains("serial port not found") {
                BoardError::NoBoard(detail)
            } else if lower.contains("denied") || lower.contains("busy") {
                BoardError::Busy(detail)
            } else if lower.contains("failed to connect")
                || lower.contains("boot mode")
                || lower.contains("sync")
                || lower.contains("timeout")
            {
                BoardError::NotInDownloadMode(detail)
            } else {
                BoardError::Lost(detail)
            }
        }
        E::SerialNotFound(_) | E::NoSerial => BoardError::NoBoard(detail),
        E::ChipDetectError(_) => BoardError::NotInDownloadMode(detail),
        E::Flashing(_) | E::RomError(_) | E::VerifyFailed | E::DigestMismatch(..) => BoardError::Lost(detail),
        _ => BoardError::Other(detail),
    }
}

/// Hands espflash's progress to the board's caller.
struct Report<'a> {
    progress: &'a mut dyn FnMut(u64),
}

impl ProgressCallbacks for Report<'_> {
    fn init(&mut self, _addr: u32, _total: usize) {}
    fn update(&mut self, current: usize) {
        (self.progress)(current as u64)
    }
    fn verifying(&mut self) {}
    fn finish(&mut self, _skipped: bool) {}
}

impl Board for EspBoard {
    fn connect(&mut self) -> Result<BoardInfo, BoardError> {
        let serial = serialport::new(&self.port, BAUD)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_secs(3))
            .open_native()
            .map_err(|e| open_error(&self.port, &e))?;
        // espflash runs its "after" reset at the end of every `write_bin_to_flash`. With a hard reset
        // there, the board would restart into half-written flash after the first part and stop
        // answering. So it stays in the loader stub between parts, and `restart` resets it once.
        let connection = Connection::new(
            serial,
            usb_info(&self.port),
            ResetAfterOperation::NoResetNoStub,
            ResetBeforeOperation::DefaultReset,
            BAUD,
        );
        // The loader stub is faster and checks each write; nothing is skipped, and every write is
        // read back and compared.
        let mut flasher = Flasher::connect(connection, true, true, false, None, None).map_err(sort)?;
        let secure_download = flasher.secure_download_mode();
        let info = flasher.device_info().map_err(sort)?;
        self.flasher = Some(flasher);
        Ok(BoardInfo {
            chip: info.chip.to_string().to_lowercase(),
            flash_bytes: u64::from(info.flash_size.size()),
            secure_download,
        })
    }

    fn erase_all(&mut self) -> Result<(), BoardError> {
        self.flasher()?.erase_flash().map_err(sort)
    }

    fn write(&mut self, offset: u32, bytes: &[u8], progress: &mut dyn FnMut(u64)) -> Result<(), BoardError> {
        let mut report = Report { progress };
        self.flasher()?
            .write_bin_to_flash(offset, bytes, &mut report)
            .map_err(sort)
    }

    fn restart(mut self: Box<Self>) -> Result<(), BoardError> {
        let mut flasher = self
            .flasher
            .take()
            .ok_or_else(|| BoardError::Other("not connected to the board".into()))?;
        // The connection's "after" setting is to stay in the stub (see `connect`), so this resets
        // the board directly, the way espflash's own hard reset does (USB-Serial-JTAG aware).
        let result = flasher.connection().reset().map_err(sort);
        // Dropping the flasher closes the port, so provisioning can open it.
        drop(flasher);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_that_wont_open_are_sorted_into_what_to_do() {
        let e = |kind, text: &str| serialport::Error::new(kind, text);
        assert!(matches!(
            open_error("COM9", &e(serialport::ErrorKind::NoDevice, "gone")),
            BoardError::NoBoard(_)
        ));
        assert!(matches!(
            open_error(
                "COM5",
                &e(
                    serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied),
                    "x"
                )
            ),
            BoardError::Busy(_)
        ));
        assert!(matches!(
            open_error("COM5", &e(serialport::ErrorKind::Unknown, "Access is denied.")),
            BoardError::Busy(_)
        ));
        assert!(matches!(
            open_error(
                "COM5",
                &e(
                    serialport::ErrorKind::Unknown,
                    "The system cannot find the file specified."
                )
            ),
            BoardError::NoBoard(_)
        ));
        assert!(matches!(
            sort(espflash::Error::SerialNotFound("COM5".into())),
            BoardError::NoBoard(_)
        ));
        assert!(matches!(sort(espflash::Error::VerifyFailed), BoardError::Lost(_)));
    }

    #[test]
    fn a_port_with_nothing_on_it_is_no_board() {
        let mut b = EspBoard::new("/dev/display-hub-no-such-port");
        assert!(matches!(b.connect(), Err(BoardError::NoBoard(_))));
        assert!(matches!(b.erase_all(), Err(BoardError::Other(_))));
    }
}
