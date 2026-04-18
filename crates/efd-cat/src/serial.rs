use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use nix::fcntl::{self, OFlag};
use nix::poll::{PollFd, PollFlags, PollTimeout};
use nix::sys::stat::Mode;
use nix::sys::termios::{self, BaudRate, SetArg, SpecialCharacterIndices};
use nix::unistd;
use tracing::{debug, info};

use crate::error::CatError;

/// Maximum length of a CAT command we'll accept from clients.
pub const MAX_CAT_CMD_LEN: usize = 64;

/// Hard cap on how long a single CAT command (write + first response
/// byte) or response-read may block. Protects the CAT task from a
/// hung radio — e.g. a mid-session USB unplug where the kernel hasn't
/// surfaced the disconnect yet and write(2) would otherwise block
/// indefinitely. 500 ms is comfortably longer than any real
/// radio-processing latency at 38400 baud.
const IO_DEADLINE: Duration = Duration::from_millis(500);

/// Wait for `fd` to be ready for `flags` (POLLIN / POLLOUT) within
/// `remaining` time. Returns `Ok(true)` if ready, `Ok(false)` on
/// timeout / EINTR, `Err` on any other poll failure.
fn wait_fd(
    fd: BorrowedFd<'_>,
    flags: PollFlags,
    remaining: Duration,
) -> Result<bool, CatError> {
    let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
    let mut fds = [PollFd::new(fd, flags)];
    let timeout = PollTimeout::try_from(ms).unwrap_or(PollTimeout::ZERO);
    match nix::poll::poll(&mut fds, timeout) {
        Ok(0) => Ok(false),
        Ok(_) => Ok(true),
        Err(nix::errno::Errno::EINTR) => Ok(false),
        Err(e) => Err(CatError::Io(std::io::Error::other(e))),
    }
}

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
        .map_err(|e| CatError::Io(std::io::Error::other(e)))?;

        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // Configure: 38400 8N1, no flow control, raw mode
        let mut tty = termios::tcgetattr(&fd)
            .map_err(|e| CatError::Io(std::io::Error::other(e)))?;

        termios::cfsetispeed(&mut tty, BaudRate::B38400)
            .map_err(|e| CatError::Io(std::io::Error::other(e)))?;
        termios::cfsetospeed(&mut tty, BaudRate::B38400)
            .map_err(|e| CatError::Io(std::io::Error::other(e)))?;

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
            .map_err(|e| CatError::Io(std::io::Error::other(e)))?;

        // Clear O_NONBLOCK now that port is configured
        let flags = fcntl::fcntl(fd.as_raw_fd(), fcntl::FcntlArg::F_GETFL)
            .map_err(|e| CatError::Io(std::io::Error::other(e)))?;
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.remove(OFlag::O_NONBLOCK);
        fcntl::fcntl(fd.as_raw_fd(), fcntl::FcntlArg::F_SETFL(oflags))
            .map_err(|e| CatError::Io(std::io::Error::other(e)))?;

        // Flush pending data
        termios::tcflush(&fd, termios::FlushArg::TCIOFLUSH)
            .map_err(|e| CatError::Io(std::io::Error::other(e)))?;

        info!(device = %device, "CAT serial port opened (38400 8N1)");

        Ok(Self {
            fd,
            device: device.to_string(),
        })
    }

    /// Send a CAT command and read the matching `;`-terminated response.
    /// Discards stale responses from previous commands.
    pub fn command(&self, cmd: &str) -> Result<String, CatError> {
        // Extract expected prefix (e.g. "IF" from "IF;", "RF" from "RF2;")
        let expected_prefix: String = cmd
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();

        // Flush input buffer
        termios::tcflush(&self.fd, termios::FlushArg::TCIFLUSH)
            .map_err(|e| CatError::Io(std::io::Error::other(e)))?;

        // Write command with an overall deadline. Without this, a
        // mid-session USB unplug where the kernel is still fielding
        // write(2) can hang the CAT task forever — poll(2) on POLLOUT
        // gives us a bounded wait.
        let mut written = 0;
        let cmd_bytes = cmd.as_bytes();
        let write_deadline = Instant::now() + IO_DEADLINE;
        while written < cmd_bytes.len() {
            let remaining = write_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CatError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "CAT write timed out after {:?} on {} ({} of {} bytes written)",
                        IO_DEADLINE, self.device, written, cmd_bytes.len()
                    ),
                )));
            }
            if !wait_fd(self.fd.as_fd(), PollFlags::POLLOUT, remaining)? {
                // Timeout or EINTR — loop and re-check deadline
                continue;
            }
            match unistd::write(&self.fd, &cmd_bytes[written..]) {
                Ok(n) => written += n,
                Err(nix::errno::Errno::EINTR) | Err(nix::errno::Errno::EAGAIN) => continue,
                Err(e) => {
                    return Err(CatError::Io(std::io::Error::other(
                        e,
                    )))
                }
            }
        }

        // Small delay for radio to process (50ms, same as EladSpectrum)
        std::thread::sleep(Duration::from_millis(50));

        // Read responses until we get one matching our command prefix.
        // Discards stale responses from previous commands.
        let mut attempts = 3;
        loop {
            let resp = self.read_response()?;
            if resp.is_empty() {
                return Ok(resp);
            }
            if resp.starts_with(&expected_prefix) {
                debug!(cmd = %cmd.trim_end_matches(';'), resp = %resp, "CAT");
                return Ok(resp);
            }
            // Stale response — discard and try again
            debug!(
                cmd = %cmd.trim_end_matches(';'),
                stale = %resp,
                "discarding stale CAT response"
            );
            attempts -= 1;
            if attempts == 0 {
                return Ok(resp); // give up, return whatever we got
            }
        }
    }

    /// Read a single `;`-terminated response from the serial port.
    ///
    /// Uses an overall deadline (`IO_DEADLINE`) rather than a retry count,
    /// so the total blocking time is bounded even when the radio is
    /// trickling bytes. Returns whatever was accumulated on timeout,
    /// matching the previous contract (empty string → caller gives up).
    fn read_response(&self) -> Result<String, CatError> {
        let mut response = Vec::with_capacity(64);
        let mut buf = [0u8; 64];
        let deadline = Instant::now() + IO_DEADLINE;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            if !wait_fd(self.fd.as_fd(), PollFlags::POLLIN, remaining)? {
                continue;
            }
            match unistd::read(self.fd.as_raw_fd(), &mut buf) {
                Ok(0) => continue,
                Ok(n) => {
                    response.extend_from_slice(&buf[..n]);
                    if let Some(pos) = response.iter().position(|&b| b == b';') {
                        response.truncate(pos + 1);
                        break;
                    }
                }
                Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    return Err(CatError::Io(std::io::Error::other(
                        e,
                    )))
                }
            }
        }

        Ok(String::from_utf8_lossy(&response).into_owned())
    }

    pub fn device(&self) -> &str {
        &self.device
    }
}
