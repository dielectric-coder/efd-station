//! DRM (Digital Radio Mondiale) decoder bridge.
//!
//! Pipes the in-process IQ stream through the vendored DREAM 2.1.1
//! decoder by way of two PipeWire null sinks:
//!
//! ```text
//! IqBlock  ──→ decimate 192k→48k ──→ pacat → sink(drm_in)
//!                                                │ (monitor)
//!                          dream -I drm_in.monitor -O drm_out -c 6
//!                                                │
//!                                          sink(drm_out).monitor → parec
//!                                                │
//!                                          AudioBlock → broadcast
//! ```
//!
//! DREAM stays unpatched beyond the hamlib cast fix — sound-card I/O
//! is the path it was designed for. On CM5 Trixie PipeWire ships by
//! default, so `pactl`/`pacat`/`parec` are available and the null-sink
//! pattern works without snd-aloop or extra config.

use std::process::Stdio;
use std::sync::Arc;

use efd_iq::IqBlock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::demod::AudioBlock;
use crate::error::DspError;
use crate::filter::FirDecimator;

/// Configuration for the DRM bridge.
#[derive(Debug, Clone)]
pub struct DrmConfig {
    /// Path to the vendored dream binary.
    pub dream_binary: String,
    /// Name of the input null sink we create.
    pub input_sink: String,
    /// Name of the output null sink we create.
    pub output_sink: String,
    /// Incoming IQ sample rate (broadcast publisher's rate).
    pub iq_input_rate: u32,
    /// Rate at which dream is driven. Must be one of 24/48/96/192 kHz
    /// (dream only accepts those). 48 kHz is the usual choice.
    pub dream_rate: u32,
    /// Taps in the anti-aliasing FIR used for the IQ→dream decimation.
    pub decim_taps: usize,
}

impl Default for DrmConfig {
    fn default() -> Self {
        Self {
            dream_binary: "dream".into(),
            input_sink: "efd_drm_in".into(),
            output_sink: "efd_drm_out".into(),
            iq_input_rate: 192_000,
            dream_rate: 48_000,
            decim_taps: 65,
        }
    }
}

/// Spawn the DRM bridge. Returns a join handle that resolves when the
/// task exits (via `cancel`, child process death, or unrecoverable I/O).
///
/// On entry, this function loads two `module-null-sink` modules via
/// `pactl`, then spawns `pacat`, `parec`, and `dream` as children. On
/// shutdown all three are killed and the sinks are unloaded in a
/// best-effort Drop guard.
pub fn spawn_drm_bridge(
    cfg: DrmConfig,
    iq_rx: broadcast::Receiver<Arc<IqBlock>>,
    audio_tx: mpsc::Sender<AudioBlock>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), DspError>> {
    tokio::spawn(async move { run_bridge(cfg, iq_rx, audio_tx, cancel).await })
}

async fn run_bridge(
    cfg: DrmConfig,
    mut iq_rx: broadcast::Receiver<Arc<IqBlock>>,
    audio_tx: mpsc::Sender<AudioBlock>,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    let factor = (cfg.iq_input_rate / cfg.dream_rate) as usize;
    if factor < 1 || cfg.iq_input_rate % cfg.dream_rate != 0 {
        return Err(DspError::Drm(format!(
            "iq_input_rate ({}) not an integer multiple of dream_rate ({})",
            cfg.iq_input_rate, cfg.dream_rate
        )));
    }

    // Load both null sinks. Module indices are held by the guard so we
    // unload them on drop even if this fn returns via `?`.
    let _sinks = NullSinks::create(&cfg.input_sink, &cfg.output_sink, cfg.dream_rate).await?;

    // `pacat` writes the decimated IQ (s16 stereo) into drm_in.
    let mut pacat = Command::new("pacat")
        .args([
            "--playback",
            "--device",
            &cfg.input_sink,
            "--channels=2",
            "--format=s16le",
            &format!("--rate={}", cfg.dream_rate),
            "--raw",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| DspError::Drm(format!("pacat spawn failed: {e}")))?;
    let mut pacat_stdin = pacat.stdin.take().expect("stdin piped");

    // `parec` reads decoded audio from drm_out.monitor.
    let mut parec = Command::new("parec")
        .args([
            "--record",
            "--device",
            &format!("{}.monitor", cfg.output_sink),
            "--channels=2",
            "--format=s16le",
            &format!("--rate={}", cfg.dream_rate),
            "--raw",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| DspError::Drm(format!("parec spawn failed: {e}")))?;
    let mut parec_stdout = parec.stdout.take().expect("stdout piped");

    // DREAM: I/Q input (0 Hz IF) from drm_in.monitor, audio to drm_out.
    let mut dream = Command::new(&cfg.dream_binary)
        .args([
            "-I",
            &format!("{}.monitor", cfg.input_sink),
            "-O",
            &cfg.output_sink,
            "-c",
            "6", // I/Q input positive, 0 Hz IF
            "--sigsrate",
            &cfg.dream_rate.to_string(),
            "--audsrate",
            &cfg.dream_rate.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| DspError::Drm(format!("dream spawn failed: {e}")))?;

    info!(
        pacat = pacat.id().unwrap_or(0),
        parec = parec.id().unwrap_or(0),
        dream = dream.id().unwrap_or(0),
        "DRM bridge started"
    );

    // IQ → pacat writer task
    let cancel_w = cancel.clone();
    let dream_rate = cfg.dream_rate;
    let iq_rate = cfg.iq_input_rate;
    let decim_taps = cfg.decim_taps;
    let writer = tokio::spawn(async move {
        let mut dec_i = FirDecimator::new(factor, decim_taps);
        let mut dec_q = FirDecimator::new(factor, decim_taps);
        let mut i_buf = Vec::with_capacity(4096);
        let mut q_buf = Vec::with_capacity(4096);
        loop {
            tokio::select! {
                biased;
                _ = cancel_w.cancelled() => {
                    debug!("DRM writer: cancelled");
                    break;
                }
                r = iq_rx.recv() => match r {
                    Ok(block) => {
                        i_buf.clear();
                        q_buf.clear();
                        for &[i, q] in &block.samples {
                            i_buf.push(i);
                            q_buf.push(q);
                        }
                        let i_dec = dec_i.process(&i_buf);
                        let q_dec = dec_q.process(&q_buf);
                        // Interleave L=I, R=Q as s16le.
                        let mut out = Vec::with_capacity(i_dec.len() * 4);
                        for (i, q) in i_dec.iter().zip(q_dec.iter()) {
                            out.extend_from_slice(&f32_to_s16(*i).to_le_bytes());
                            out.extend_from_slice(&f32_to_s16(*q).to_le_bytes());
                        }
                        if pacat_stdin.write_all(&out).await.is_err() {
                            debug!("DRM writer: pacat stdin closed");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "DRM writer: IQ lagged");
                        dec_i.reset();
                        dec_q.reset();
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!("DRM writer: IQ channel closed");
                        break;
                    }
                }
            }
        }
        let _ = pacat_stdin.shutdown().await;
        let _ = (iq_rate, dream_rate); // silence unused warnings when features change
    });

    // parec → AudioBlock task
    let cancel_r = cancel.clone();
    let audio_tx_r = audio_tx.clone();
    let dream_rate_r = cfg.dream_rate;
    let reader = tokio::spawn(async move {
        // Read in chunks that yield ~20 ms of audio each, so the WS
        // downstream and ALSA sink see comparable frame sizes to the
        // in-process demod.
        const FRAMES_PER_CHUNK: usize = 960;                         // 20 ms @ 48 kHz
        const BYTES_PER_CHUNK: usize = FRAMES_PER_CHUNK * 2 * 2;     // stereo s16
        let mut buf = vec![0u8; BYTES_PER_CHUNK];
        let start = std::time::Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = cancel_r.cancelled() => {
                    debug!("DRM reader: cancelled");
                    break;
                }
                res = parec_stdout.read_exact(&mut buf) => match res {
                    Ok(_) => {
                        // Downmix stereo s16 → mono f32 to match the rest of
                        // the demod pipeline's AudioBlock contract.
                        let mut mono = Vec::with_capacity(FRAMES_PER_CHUNK);
                        for chunk in buf.chunks_exact(4) {
                            let l = i16::from_le_bytes([chunk[0], chunk[1]]);
                            let r = i16::from_le_bytes([chunk[2], chunk[3]]);
                            let mix = (l as f32 + r as f32) * 0.5 / 32768.0;
                            mono.push(mix);
                        }
                        let blk = AudioBlock {
                            samples: mono,
                            sample_rate: dream_rate_r,
                            timestamp_us: start.elapsed().as_micros() as u64,
                        };
                        if audio_tx_r.send(blk).await.is_err() {
                            debug!("DRM reader: audio_tx closed");
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        debug!("DRM reader: parec stdout EOF");
                        break;
                    }
                    Err(e) => {
                        warn!("DRM reader: parec stdout: {e}");
                        break;
                    }
                }
            }
        }
    });

    // Supervise: cancel when any child dies or cancel fires. Then tear down.
    let rc = tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(()),
        s = dream.wait() => {
            warn!(status = ?s, "dream exited unexpectedly");
            Err(DspError::Drm("dream subprocess exited".into()))
        }
        s = pacat.wait() => {
            warn!(status = ?s, "pacat exited unexpectedly");
            Err(DspError::Drm("pacat subprocess exited".into()))
        }
        s = parec.wait() => {
            warn!(status = ?s, "parec exited unexpectedly");
            Err(DspError::Drm("parec subprocess exited".into()))
        }
    };

    // Teardown: fire the local cancel so the two inner tasks exit, then
    // kill any children still alive, then await everything.
    cancel.cancel();
    let _ = dream.kill().await;
    let _ = pacat.kill().await;
    let _ = parec.kill().await;
    let _ = writer.await;
    let _ = reader.await;

    info!("DRM bridge stopped");
    rc
}

/// Clamp and convert an f32 (-1.0..1.0) to s16le.
#[inline]
fn f32_to_s16(v: f32) -> i16 {
    let scaled = (v * 32767.0).round();
    scaled.clamp(-32768.0, 32767.0) as i16
}

/// RAII guard for the two PipeWire null sinks.
struct NullSinks {
    in_module: u32,
    out_module: u32,
}

impl NullSinks {
    async fn create(in_name: &str, out_name: &str, rate: u32) -> Result<Self, DspError> {
        let in_module = pactl_load_null_sink(in_name, rate).await?;
        let out_module = match pactl_load_null_sink(out_name, rate).await {
            Ok(idx) => idx,
            Err(e) => {
                // Don't leak the first module if the second fails.
                let _ = pactl_unload_module(in_module).await;
                return Err(e);
            }
        };
        Ok(Self {
            in_module,
            out_module,
        })
    }
}

impl Drop for NullSinks {
    fn drop(&mut self) {
        // Best-effort synchronous cleanup — Drop can't await, so just
        // fire-and-forget via std::process.
        let _ = std::process::Command::new("pactl")
            .args(["unload-module", &self.in_module.to_string()])
            .status();
        let _ = std::process::Command::new("pactl")
            .args(["unload-module", &self.out_module.to_string()])
            .status();
    }
}

async fn pactl_load_null_sink(name: &str, rate: u32) -> Result<u32, DspError> {
    let out = Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={name}"),
            &format!("rate={rate}"),
            "channels=2",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| DspError::Drm(format!("pactl load-module: {e}")))?;
    if !out.status.success() {
        return Err(DspError::Drm(format!(
            "pactl load-module failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim()
        .parse::<u32>()
        .map_err(|_| DspError::Drm(format!("pactl load-module returned non-integer: {s:?}")))
}

async fn pactl_unload_module(idx: u32) -> Result<(), DspError> {
    let status = Command::new("pactl")
        .args(["unload-module", &idx.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| DspError::Drm(format!("pactl unload-module: {e}")))?;
    if !status.success() {
        warn!(module = idx, "pactl unload-module returned non-zero");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_s16_clamping() {
        assert_eq!(f32_to_s16(0.0), 0);
        assert_eq!(f32_to_s16(1.0), 32767);
        assert_eq!(f32_to_s16(-1.0), -32767);
        assert_eq!(f32_to_s16(2.0), 32767); // clamp hi
        assert_eq!(f32_to_s16(-2.0), -32768); // clamp lo
    }

    #[test]
    fn default_config_rates_are_compatible() {
        let cfg = DrmConfig::default();
        assert_eq!(cfg.iq_input_rate % cfg.dream_rate, 0);
        assert!(matches!(
            cfg.dream_rate,
            24_000 | 48_000 | 96_000 | 192_000
        ));
    }
}
