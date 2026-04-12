use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::backend::{FdmDuoConfig, SourceConfig};
use crate::device::{convert_samples, FdmDuo, USB_BUFFER_SIZE};
use crate::error::IqError;
use crate::IqBlock;

/// Spawn the IQ capture task for the configured source. Dispatches to the
/// per-backend implementation. Returns a JoinHandle that resolves when the
/// task exits. For sources that provide no IQ (portable-radio) or backends
/// that aren't implemented yet, the handle resolves immediately with an
/// error — the caller is expected to have checked `capabilities().has_iq`
/// before calling.
///
/// Publishes the LO center frequency via `center_freq_tx`. For backends
/// where the LO is fixed or set elsewhere, `center_freq_tx` may remain at
/// its initial value.
pub fn spawn_source(
    cfg: SourceConfig,
    tx: broadcast::Sender<Arc<IqBlock>>,
    center_freq_tx: watch::Sender<u64>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), IqError>> {
    match cfg {
        SourceConfig::FdmDuo(c) => {
            tokio::task::spawn_blocking(move || run_fdmduo(c, tx, center_freq_tx, cancel))
        }
        SourceConfig::PortableRadio(_) => {
            let kind = efd_proto::SourceKind::PortableRadio;
            tokio::spawn(async move { Err(IqError::SourceHasNoIq(kind)) })
        }
        other => {
            let kind = other.kind();
            tokio::spawn(async move { Err(IqError::BackendNotImplemented(kind)) })
        }
    }
}

fn run_fdmduo(
    cfg: FdmDuoConfig,
    tx: broadcast::Sender<Arc<IqBlock>>,
    center_freq_tx: watch::Sender<u64>,
    cancel: CancellationToken,
) -> Result<(), IqError> {
    let dev = FdmDuo::open_with_ids(cfg.vendor_id, cfg.product_id)?;
    dev.start_streaming()?;

    // Read initial FPGA center frequency
    match dev.read_frequency() {
        Ok(freq) => {
            info!("IQ center frequency: {freq} Hz");
            let _ = center_freq_tx.send(freq);
        }
        Err(e) => warn!("could not read FPGA frequency: {e}"),
    }

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
                // Periodically re-read FPGA center frequency (LO may change)
                if block_count % 500 == 0 {
                    if let Ok(freq) = dev.read_frequency() {
                        let _ = center_freq_tx.send(freq);
                    }
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
