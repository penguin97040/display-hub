//! Reading a passphrase: from the terminal with echo turned off, or, when standard input isn't a
//! terminal (a script or a test), one line from standard input. Never from the command line.

use std::io::{BufRead, IsTerminal, Read, Write};

use firmware_package::keyfile::MAX_PASSPHRASE_BYTES;
use zeroize::{Zeroize, Zeroizing};

/// Asks once.
pub fn ask(prompt: &str) -> Result<Zeroizing<String>, String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return read_line();
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let line = {
        let _quiet = NoEcho::start()?;
        read_line()
    };
    #[cfg(windows)]
    eprintln!();
    line
}

/// Asks twice on a terminal, so a typing mistake doesn't lock the maker out of a new key. From
/// standard input, reads the one line.
pub fn ask_new(prompt: &str) -> Result<Zeroizing<String>, String> {
    let first = ask(prompt)?;
    if std::io::stdin().is_terminal() {
        let second = ask("Type it again: ")?;
        if *first != *second {
            return Err("the two passphrases don't match".into());
        }
    }
    Ok(first)
}

/// One line, without its line ending.
///
/// The buffer is made at its full size first, so it never moves and leaves no copy behind to be
/// zeroed. Standard input's own buffer, inside the standard library, may still hold the line until
/// it is overwritten: there is no portable way to clear it.
fn read_line() -> Result<Zeroizing<String>, String> {
    // Room for the longest passphrase, a CR LF, and one byte to notice anything longer.
    let limit = (MAX_PASSPHRASE_BYTES + 3) as u64;
    let mut buf = Zeroizing::new(Vec::with_capacity(limit as usize));
    std::io::stdin()
        .lock()
        .take(limit)
        .read_until(b'\n', &mut buf)
        .map_err(|e| format!("couldn't read the passphrase: {e}"))?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    } else if buf.len() as u64 >= limit {
        return Err(format!(
            "the passphrase is longer than {MAX_PASSPHRASE_BYTES} bytes"
        ));
    }
    if buf.is_empty() {
        return Err("no passphrase given".into());
    }
    match String::from_utf8(std::mem::take(&mut *buf)) {
        Ok(s) => Ok(Zeroizing::new(s)),
        Err(e) => {
            e.into_bytes().zeroize();
            Err("the passphrase isn't valid UTF-8 text".into())
        }
    }
}

/// Turns the terminal's echo off until dropped.
struct NoEcho {
    #[cfg(unix)]
    saved: libc::termios,
    #[cfg(windows)]
    saved: (windows_sys::Win32::Foundation::HANDLE, u32),
}

#[cfg(unix)]
impl NoEcho {
    fn start() -> Result<Self, String> {
        // SAFETY: `termios` is plain data that tcgetattr fills in; fd 0 is standard input, which the
        // caller has checked is a terminal.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return Err("couldn't turn off the terminal's echo".into());
            }
            let mut quiet = saved;
            // Keep echoing the final newline, so the next output starts on a fresh line.
            quiet.c_lflag &= !libc::ECHO;
            quiet.c_lflag |= libc::ECHONL;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet) != 0 {
                return Err("couldn't turn off the terminal's echo".into());
            }
            Ok(Self { saved })
        }
    }
}

#[cfg(unix)]
impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: restores the settings read in `start`, on the same descriptor.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

#[cfg(windows)]
impl NoEcho {
    fn start() -> Result<Self, String> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
        };
        // SAFETY: plain console calls on this process's standard input handle, with a valid pointer
        // for the mode.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return Err("couldn't turn off the console's echo".into());
            }
            let mut mode = 0u32;
            if GetConsoleMode(handle, &mut mode) == 0
                || SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) == 0
            {
                return Err("couldn't turn off the console's echo".into());
            }
            Ok(Self {
                saved: (handle, mode),
            })
        }
    }
}

#[cfg(windows)]
impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: restores the mode read in `start`, on the same handle.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleMode(self.saved.0, self.saved.1);
        }
    }
}
