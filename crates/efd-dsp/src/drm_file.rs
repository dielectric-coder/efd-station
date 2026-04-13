//! DRM bridge variant that plays a local audio-IF file through DREAM.
//!
//! Same downstream plumbing as [`crate::drm`] — DREAM is spawned with
//! `drm_in` / `drm_out` PipeWire null sinks, the reader task pulls
//! decoded audio out of `drm_out.monitor` and publishes `AudioBlock`s.
//! The upstream half is replaced: instead of decimating live IQ and
//! writing stereo L=I R=Q into `drm_in` with `pacat`, this variant
//! spawns `paplay FILE --device=drm_in` and lets PipeWire handle the
//! format conversion.
//!
//! DREAM is invoked **without** `-c 6`, so it interprets the input as
//! real audio-IF (the format DREAM's bundled FLAC samples ship in).
//! This is a test-only path — the production `drm.rs` bridge is the
//! only one that operates on live radio IQ.
//!
//! The bridge exits when `paplay` finishes (file fully played), when
//! the cancellation token fires, or when any child process dies.

use std::path::PathBuf;
use std::process::Stdio;

use efd_proto::DrmStatus;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::demod::AudioBlock;
use crate::drm::{
    parse_tui_line, spawn_with_timeout, strip_ansi, DrmConfig, DrmStatusExt, NullSinks,
};
use crate::error::DspError;

/// Handles returned from [`spawn_drm_file_bridge`].
pub struct DrmFileHandles {
    /// Resolves when the bridge task exits (paplay finished, cancelled,
    /// or a child died).
    pub join: JoinHandle<Result<(), DspError>>,
    /// Live DRM decoder status, same semantics as [`crate::drm::DrmHandles`].
    pub status_rx: watch::Receiver<Option<DrmStatus>>,
}

/// Spawn a file-playback DRM test bridge.
///
/// See the module docs for the full pipeline. `audio_tx` receives the
/// decoded audio in the same `AudioBlock` format as the analog demod, so
/// the rest of the server pipeline (opus encoder → WS downstream) works
/// unchanged.
pub fn spawn_drm_file_bridge(
    cfg: DrmConfig,
    file_path: PathBuf,
    audio_tx: mpsc::Sender<AudioBlock>,
    cancel: CancellationToken,
) -> DrmFileHandles {
    let (status_tx, status_rx) = watch::channel::<Option<DrmStatus>>(None);
    let join = tokio::spawn(async move {
        run_file_bridge(cfg, file_path, audio_tx, status_tx, cancel).await
    });
    DrmFileHandles { join, status_rx }
}

async fn run_file_bridge(
    cfg: DrmConfig,
    file_path: PathBuf,
    audio_tx: mpsc::Sender<AudioBlock>,
    status_tx: watch::Sender<Option<DrmStatus>>,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    if !file_path.exists() {
        return Err(DspError::Drm(format!(
            "drm-file-test: input file does not exist: {}",
            file_path.display()
        )));
    }

    // Same two null sinks as the live bridge; `paplay` resamples into the
    // sink's rate so the choice here must match DREAM's expected rate.
    let _sinks = NullSinks::create(&cfg.input_sink, &cfg.output_sink, cfg.dream_rate).await?;

    // `paplay` reads the WAV/FLAC and streams it into the input sink.
    // It handles format decoding natively via libsndfile, so no Rust-side
    // FLAC dependency is needed.
    let paplay = spawn_with_timeout("paplay", || {
        Command::new("paplay")
            .arg("--device")
            .arg(&cfg.input_sink)
            .arg(&file_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    })
    .await?;

    // `parec` reads decoded audio from drm_out.monitor — identical to the
    // live bridge's reader.
    let mut parec = spawn_with_timeout("parec", || {
        Command::new("parec")
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
    })
    .await?;
    let mut parec_stdout = parec
        .stdout
        .take()
        .ok_or_else(|| DspError::Drm("parec: stdout not piped".into()))?;

    // DREAM in **audio-IF mode** (no `-c 6`): interprets input as real
    // audio, which matches the FLAC IF recordings DREAM ships with.
    let mut dream = spawn_with_timeout("dream", || {
        Command::new(&cfg.dream_binary)
            .args([
                "-I",
                &format!("{}.monitor", cfg.input_sink),
                "-O",
                &cfg.output_sink,
                "--sigsrate",
                &cfg.dream_rate.to_string(),
                "--audsrate",
                &cfg.dream_rate.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    })
    .await?;
    let dream_stdout = dream
        .stdout
        .take()
        .ok_or_else(|| DspError::Drm("dream: stdout not piped".into()))?;

    info!(
        file = %file_path.display(),
        paplay = paplay.id().unwrap_or(0),
        parec = parec.id().unwrap_or(0),
        dream = dream.id().unwrap_or(0),
        "DRM file-test bridge started"
    );

    // Reader: parec s16 stereo → mono f32 AudioBlock → mpsc.
    let cancel_r = cancel.clone();
    let audio_tx_r = audio_tx.clone();
    let dream_rate_r = cfg.dream_rate;
    let reader = tokio::spawn(async move {
        const FRAMES_PER_CHUNK: usize = 960; // 20 ms @ 48 kHz
        const BYTES_PER_CHUNK: usize = FRAMES_PER_CHUNK * 2 * 2; // stereo s16
        let mut buf = vec![0u8; BYTES_PER_CHUNK];
        let start = std::time::Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = cancel_r.cancelled() => {
                    debug!("DRM file reader: cancelled");
                    break;
                }
                res = parec_stdout.read_exact(&mut buf) => match res {
                    Ok(_) => {
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
                            debug!("DRM file reader: audio_tx closed");
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        debug!("DRM file reader: parec EOF");
                        break;
                    }
                    Err(e) => {
                        warn!("DRM file reader: parec: {e}");
                        break;
                    }
                }
            }
        }
    });

    // TUI parser: dream stdout → DrmStatus frames published on `status_tx`.
    let cancel_s = cancel.clone();
    let bridge_start = std::time::Instant::now();
    let status = tokio::spawn(async move {
        let mut rdr = BufReader::new(dream_stdout);
        let mut acc = DrmStatus::empty();
        let mut line = String::new();
        loop {
            line.clear();
            tokio::select! {
                biased;
                _ = cancel_s.cancelled() => {
                    debug!("DRM file TUI: cancelled");
                    break;
                }
                res = rdr.read_line(&mut line) => match res {
                    Ok(0) => { debug!("DRM file TUI: dream stdout EOF"); break; }
                    Ok(_) => {
                        let clean = strip_ansi(line.trim_end_matches(['\r', '\n']));
                        if parse_tui_line(&clean, &mut acc) {
                            acc.timestamp_us = bridge_start.elapsed().as_micros() as u64;
                            let _ = status_tx.send(Some(acc.clone()));
                            acc = DrmStatus::empty();
                        }
                    }
                    Err(e) => { warn!("DRM file TUI: read: {e}"); break; }
                }
            }
        }
    });

    // Wait on paplay specifically — this is the test's natural end.
    // Take ownership via a dedicated task so we can also react to cancel.
    let cancel_p = cancel.clone();
    let paplay_wait = tokio::spawn(async move {
        let mut paplay = paplay;
        tokio::select! {
            biased;
            _ = cancel_p.cancelled() => {
                let _ = paplay.kill().await;
                debug!("paplay killed by cancel");
            }
            res = paplay.wait() => {
                match res {
                    Ok(status) if status.success() => {
                        info!("paplay finished — file fully streamed");
                    }
                    Ok(status) => {
                        warn!("paplay exited with non-zero status: {status}");
                    }
                    Err(e) => {
                        warn!("paplay wait error: {e}");
                    }
                }
            }
        }
    });

    // Bridge main loop: exit on cancel OR when paplay finishes. Give DREAM
    // a short grace period after paplay exits so the tail of the audio
    // clears drm_out.monitor before we tear down.
    let rc: Result<(), DspError> = tokio::select! {
        _ = cancel.cancelled() => {
            debug!("DRM file bridge: cancelled");
            Ok(())
        }
        _ = paplay_wait => {
            info!("paplay done; draining DREAM for 1s then stopping");
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            Ok(())
        }
    };

    // Teardown: cancel inner tasks, kill remaining children, join.
    cancel.cancel();
    let _ = dream.kill().await;
    let _ = parec.kill().await;
    let _ = reader.await;
    let _ = status.await;

    info!("DRM file-test bridge stopped");
    rc
}
