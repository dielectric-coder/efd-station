use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

use crate::error::CatError;

/// A TCP connection to rigctld that sends commands and reads `;`-terminated responses.
pub struct RigctldConn {
    stream: TcpStream,
}

impl RigctldConn {
    /// Connect to rigctld at `host:port` with TCP_NODELAY.
    pub async fn connect(host: &str, port: u16) -> Result<Self, CatError> {
        let addr = format!("{host}:{port}");
        let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .map_err(|_| {
                CatError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connect to {addr} timed out"),
                ))
            })??;

        stream.set_nodelay(true)?;
        debug!("connected to rigctld at {addr}");
        Ok(Self { stream })
    }

    /// Send a raw CAT command and read until `;` terminator.
    /// Returns the full response including the `;`.
    pub async fn command(&mut self, cmd: &str) -> Result<String, CatError> {
        self.stream.write_all(cmd.as_bytes()).await?;

        let mut buf = [0u8; 256];
        let mut response = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(CatError::BadResponse(format!(
                    "timeout waiting for ';' (got: {response})"
                )));
            }

            let n = tokio::time::timeout(remaining, self.stream.read(&mut buf)).await;
            match n {
                Ok(Ok(0)) => return Err(CatError::Disconnected),
                Ok(Ok(n)) => {
                    response.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if response.contains(';') {
                        // Trim to first `;`
                        if let Some(pos) = response.find(';') {
                            response.truncate(pos + 1);
                        }
                        return Ok(response);
                    }
                }
                Ok(Err(e)) => return Err(CatError::Io(e)),
                Err(_) => {
                    return Err(CatError::BadResponse(format!(
                        "timeout waiting for ';' (got: {response})"
                    )));
                }
            }
        }
    }
}
