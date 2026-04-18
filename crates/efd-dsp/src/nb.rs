//! Pre-IF noise blanker (IQ domain).
//!
//! The `NB` box in the pipeline drawio — sits between the IQ source
//! and `IQ → IF`, operating on the complex baseband stream. Distinct
//! from [`AudioDsp`]'s `DNB` stage, which does a similar job in the
//! audio domain after demod.
//!
//! Phase 3a: pass-through with an enable flag, the topology wiring
//! point. Real impulse-noise math lands in a later commit; a typical
//! implementation tracks the envelope magnitude, thresholds against a
//! running mean, and replaces outliers with a gated / interpolated
//! value. Plenty of references — the DSP primitive itself isn't hard.
//!
//! [`AudioDsp`]: crate::AudioDsp
use std::sync::Arc;

use efd_iq::IqBlock;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use crate::error::DspError;

/// Configuration for [`spawn_noise_blanker`].
#[derive(Debug, Clone)]
pub struct NoiseBlankerConfig {
    /// Initial enable state. `false` means pass-through, equivalent
    /// to the block being absent.
    pub enabled: bool,
}

impl Default for NoiseBlankerConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// Spawn the noise-blanker task.
///
/// Subscribes to `iq_in` (raw IQ from the capture driver) and
/// publishes to `iq_out` (clean IQ consumed by the IF demod). In
/// pass-through / disabled mode the input `Arc<IqBlock>` is
/// forwarded zero-copy; the enabled path clones the block before
/// mutating so the raw-IQ consumer (FFT) keeps a pristine view.
///
/// The `enabled_rx` watch channel lets the WS upstream toggle the
/// blanker at runtime without tearing the pipeline down.
pub fn spawn_noise_blanker(
    iq_in: broadcast::Receiver<Arc<IqBlock>>,
    iq_out: broadcast::Sender<Arc<IqBlock>>,
    enabled_rx: tokio::sync::watch::Receiver<bool>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), DspError>> {
    tokio::spawn(async move { run(iq_in, iq_out, enabled_rx, cancel).await })
}

async fn run(
    mut iq_in: broadcast::Receiver<Arc<IqBlock>>,
    iq_out: broadcast::Sender<Arc<IqBlock>>,
    enabled_rx: tokio::sync::watch::Receiver<bool>,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    debug!("NB task started");
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(DspError::Cancelled),
            r = iq_in.recv() => {
                let block = match r {
                    Ok(b) => b,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "NB: input broadcast lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err(DspError::ChannelClosed),
                };
                let out = if *enabled_rx.borrow() {
                    // Enabled branch — today identical to pass-through
                    // after cloning. The clone-point is where the real
                    // blanker will operate on `out.samples` without
                    // disturbing the raw-IQ subscriber.
                    let mut cloned: IqBlock = (*block).clone();
                    blank(&mut cloned.samples);
                    Arc::new(cloned)
                } else {
                    block
                };
                if iq_out.send(out).is_err() {
                    trace!("NB: iq_out has no subscribers");
                }
            }
        }
    }
}

/// Impulse-noise gate. Phase 3a stub — no-op. The shape is set up so
/// the later implementation has exactly one place to edit without
/// touching the pipeline wiring.
fn blank(_samples: &mut [[f32; 2]]) {
    // TODO (phase 3b-DSP): envelope-threshold impulse blanker.
}
