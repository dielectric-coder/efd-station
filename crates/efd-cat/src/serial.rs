use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use nix::fcntl::{self, OFlag};
use nix::sys::stat::Mode;
use nix::sys::termios::{self, BaudRate, SetArg, SpecialCharacterIndices};
use nix::unistd;
use tracing::{debug, info};

use crate::error::CatError;

/// Maximum length of a CAT command we'll accept from clients.
pub const MAX_CAT_CMD_LEN: usize = 64;

/// Direct serial connection to the FDM-DUO CAT port.
/// Ported from EladSpectrum cat_control.c — 38400 8N1, raw mode.
pub struct SerialPort {
    fd: OwnedFd,
    device: String,
}

impl SerialPort {
    /// Open and configure the serial port at 38400 8N1.
    pub fn open(device: &str) -> Result<Self, CatError> {
        let path = Path::new(device);
        if !path.exists() {
            return Err(CatError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("serial device not found: {device}"),
            )));
        }

        // Open with O_RDWR | O_NOCTTY | O_NONBLOCK (clear NONBLOCK after config)
        let raw_fd = fcntl::open(
            path,
            OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // Configure: 38400 8N1, no flow control, raw mode
        let mut tty = termios::tcgetattr(&fd)
            .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        termios::cfsetispeed(&mut tty, BaudRate::B38400)
            .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        termios::cfsetospeed(&mut tty, BaudRate::B38400)
            .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        // 8N1, no flow control
        tty.control_flags.remove(termios::ControlFlags::PARENB);
        tty.control_flags.remove(termios::ControlFlags::CSTOPB);
        tty.control_flags.remove(termios::ControlFlags::CSIZE);
        tty.control_flags.insert(termios::ControlFlags::CS8);
        tty.control_flags.remove(termios::ControlFlags::CRTSCTS);
        tty.control_flags.insert(termios::ControlFlags::CREAD);
        tty.control_flags.insert(termios::ControlFlags::CLOCAL);

        // Raw input
        tty.local_flags.remove(
            termios::LocalFlags::ICANON
                | termios::LocalFlags::ECHO
                | termios::LocalFlags::ECHOE
                | termios::LocalFlags::ISIG,
        );
        tty.input_flags.remove(
            termios::InputFlags::IXON
                | termios::InputFlags::IXOFF
                | termios::InputFlags::IXANY
                | termios::InputFlags::IGNBRK
                | termios::InputFlags::BRKINT
                | termios::InputFlags::PARMRK
                | termios::InputFlags::ISTRIP
                | termios::InputFlags::INLCR
                | termios::InputFlags::IGNCR
                | termios::InputFlags::ICRNL,
        );

        // Raw output
        tty.output_flags.remove(termios::OutputFlags::OPOST);

        // Read timeout: 100ms (VTIME=1 in deciseconds)
        tty.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
        tty.control_chars[SpecialCharacterIndices::VTIME as usize] = 1;

        termios::tcsetattr(&fd, SetArg::TCSANOW, &tty)
            .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        // Clear O_NONBLOCK now that port is configured
        let flags = fcntl::fcntl(fd.as_raw_fd(), fcntl::FcntlArg::F_GETFL)
            .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.remove(OFlag::O_NONBLOCK);
        fcntl::fcntl(fd.as_raw_fd(), fcntl::FcntlArg::F_SETFL(oflags))
            .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        // Flush pending data
        termios::tcflush(&fd, termios::FlushArg::TCIOFLUSH)
            .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        info!(device = %device, "CAT serial port opened (38400 8N1)");

        Ok(Self {
            fd,
            device: device.to_string(),
        })
    }

    /// Send a CAT command and read the `;`-terminated response.
    /// Uses nix::unistd::read/write directly — no unsafe File wrappers.
    pub fn command(&self, cmd: &str) -> Result<String, CatError> {
        // Flush input
        termios::tcflush(&self.fd, termios::FlushArg::TCIFLUSH)
            .map_err(|e| CatError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        // Write command using nix::unistd::write
        let mut written = 0;
        let cmd_bytes = cmd.as_bytes();
        while written < cmd_bytes.len() {
            match unistd::write(&self.fd, &cmd_bytes[written..]) {
                Ok(n) => written += n,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    return Err(CatError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e,
                    )))
                }
            }
        }

        // Small delay for radio to process (50ms, same as EladSpectrum)
        std::thread::sleep(Duration::from_millis(50));

        // Read response until ';' using nix::unistd::read
        let mut response = Vec::with_capacity(64);
        let mut buf = [0u8; 64];
        let mut retries = 10;

        loop {
            match unistd::read(self.fd.as_raw_fd(), &mut buf) {
                Ok(0) => {
                    retries -= 1;
                    if retries == 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(n) => {
                    response.extend_from_slice(&buf[..n]);
                    if let Some(pos) = response.iter().position(|&b| b == b';') {
                        response.truncate(pos + 1);
                        break;
                    }
                }
                Err(nix::errno::Errno::EAGAIN) => {
                    retries -= 1;
                    if retries == 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    return Err(CatError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e,
                    )))
                }
            }
        }

        let resp = String::from_utf8_lossy(&response);
        debug!(cmd = %cmd.trim_end_matches(';'), resp = %resp, "CAT");
        Ok(resp.into_owned())
    }

    pub fn device(&self) -> &str {
        &self.device
    }
}
