//! Per-device IQ capture drivers. Each driver is feature-gated and exposes a
//! `spawn(cfg, tx, center_freq_tx, cancel) -> JoinHandle` entry point used by
//! the top-level `spawn_source` dispatcher in `lib.rs`.

#[cfg(feature = "fdm-duo")]
pub mod fdm_duo;
