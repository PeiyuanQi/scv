//! Reading answers from the person at the terminal: plain lines, and secrets
//! with echo turned off. Piped stdin works too, for scripts.

use std::io::{BufRead as _, IsTerminal as _, Read as _, Write as _};

use anyhow::{Result, bail};
use scv_tools::stores::{MAX_FIELD_BYTES, validate_secret};

/// Read one non-secret line, from the terminal or piped stdin.
pub(crate) fn prompt_line(prompt: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        eprint!("{prompt}: ");
        std::io::stderr().flush().ok();
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

/// Read a secret from the terminal without echo, or from piped stdin.
pub(crate) fn read_secret(prompt: &str) -> Result<String> {
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.is_terminal() {
        eprint!("{prompt}: ");
        std::io::stderr().flush().ok();
        let _echo = EchoOff::new()?;
        stdin.lock().read_line(&mut line)?;
        eprintln!();
    } else {
        stdin
            .lock()
            .take(MAX_FIELD_BYTES as u64 + 2)
            .read_line(&mut line)?;
    }
    let secret = line.trim().to_owned();
    validate_secret(&secret)?;
    Ok(secret)
}

/// Terminal echo disabled for the guard's lifetime.
struct EchoOff(libc::termios);

impl EchoOff {
    fn new() -> Result<Self> {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the termios struct for a valid descriptor.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) } != 0 {
            bail!(
                "read terminal settings: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: tcgetattr succeeded, so the struct is initialized.
        let original = unsafe { termios.assume_init() };
        let mut silent = original;
        silent.c_lflag &= !libc::ECHO;
        // SAFETY: a valid descriptor and a termios derived from its own settings.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const silent) } != 0 {
            bail!("disable terminal echo: {}", std::io::Error::last_os_error());
        }
        Ok(Self(original))
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restores the settings read from the same descriptor.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.0) };
    }
}
