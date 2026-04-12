//! Multi-backend IQ capture.
//!
//! Each supported device has its own driver module under `drivers/`, gated
//! behind a cargo feature. The `spawn_source` function dispatches to the
//! active driver based on [`SourceConfig`]. When driver #2 arrives, this
//! dispatch will be generalized behind an `IqSource` trait.

use std::sync::Arc;

use efd_proto::SourceKind;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod drivers;
pub mod error;
pub mod source;
pub mod types;

pub use error::IqError;
pub use source::{
    FdmDuoConfig, HackRfConfig, PortableRadioConfig, RspDxConfig, RtlSdrConfig,
    SourceCapabilities, SourceConfig,
};
pub use types::IqBlock;

/// Spawn the IQ capture task for the configured source. Dispatches to the
/// matching driver under `drivers/`. For sources that provide no IQ
/// (portable-radio) or backends whose driver feature is disabled, the handle
/// resolves immediately with an error — the caller is expected to have
/// checked `capabilities().has_iq` before calling.
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
        #[cfg(feature = "fdm-duo")]
        SourceConfig::FdmDuo(c) => drivers::fdm_duo::spawn(c, tx, center_freq_tx, cancel),
        #[cfg(not(feature = "fdm-duo"))]
        SourceConfig::FdmDuo(_) => {
            let _ = (tx, center_freq_tx, cancel);
            spawn_not_implemented(SourceKind::FdmDuo)
        }
        SourceConfig::PortableRadio(_) => {
            let _ = (tx, center_freq_tx, cancel);
            tokio::spawn(async move { Err(IqError::SourceHasNoIq(SourceKind::PortableRadio)) })
        }
        other => {
            let _ = (tx, center_freq_tx, cancel);
            spawn_not_implemented(other.kind())
        }
    }
}

fn spawn_not_implemented(kind: SourceKind) -> JoinHandle<Result<(), IqError>> {
    tokio::spawn(async move { Err(IqError::BackendNotImplemented(kind)) })
}
