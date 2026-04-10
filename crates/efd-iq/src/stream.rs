use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::device::{convert_samples, FdmDuo, USB_BUFFER_SIZE};
use crate::error::IqError;
use crate::IqBlock;

/// Configuration for the IQ capture task.
#[derive(Debug, Clone)]
pub struct IqConfig {
    /// USB vendor ID (default: 0x1721).
    pub vendor_id: u16,
    /// USB product ID (default: 0x061a).
    pub product_id: u16,
}

impl Default for IqConfig {
    fn default() -> Self {
        Self {
            vendor_id: crate::device::ELAD_VENDOR_ID,
            product_id: crate::device::ELAD_PRODUCT_ID,
        }
    }
}

/// Spawn a blocking task that opens the FDM-DUO, streams IQ data, and
/// publishes `Arc<IqBlock>` on the broadcast channel.
///
/// The task runs until `cancel` is triggered or a fatal USB error occurs.
pub fn spawn_iq_capture(
    config: IqConfig,
    tx: broadcast::Sender<Arc<IqBlock>>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), IqError>> {
    tokio::task::spawn_blocking(move || run_capture(config, tx, cancel))
}

fn run_capture(
    config: IqConfig,
    tx: broadcast::Sender<Arc<IqBlock>>,
    cancel: CancellationToken,
) -> Result<(), IqError> {
    let dev = FdmDuo::open_with_ids(config.vendor_id, config.product_id)?;
    dev.start_streaming()?;

    info!("IQ capture started (clock={})", dev.effective_clock());

    let start = Instant::now();
    let mut buf = vec![0u8; USB_BUFFER_SIZE];
    let mut block_count: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            info!("IQ capture cancelled after {block_count} blocks");
            return Err(IqError::Cancelled);
        }

        match dev.bulk_read(&mut buf) {
            Ok(n) if n > 0 => {
                let samples = convert_samples(&buf[..n]);
                let timestamp_us = start.elapsed().as_micros() as u64;

                let block = Arc::new(IqBlock {
                    samples,
                    timestamp_us,
                });

                // If no receivers, silently drop the block
                let _ = tx.send(block);

                block_count += 1;
                if block_count % 1000 == 0 {
                    debug!(block_count, "IQ blocks captured");
                }
            }
            Ok(_) => {
                // Zero-length read — unusual but not fatal
                warn!("zero-length USB read");
            }
            Err(rusb::Error::Timeout) => {
                // Timeout — retry
                continue;
            }
            Err(rusb::Error::NoDevice) => {
                error!("FDM-DUO disconnected");
                return Err(IqError::Usb(rusb::Error::NoDevice));
            }
            Err(e) => {
                error!("USB read error: {e}");
                return Err(IqError::Usb(e));
            }
        }
    }
}
