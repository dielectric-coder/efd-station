//! REC feature (phase 4).
//!
//! Captures either IQ or audio to a file on disk, one recording at
//! a time. The file format is deliberately simple so the captured
//! stream can be replayed through the pipeline later without a
//! special reader:
//!
//! - **IQ** → raw `f32` interleaved `[I, Q]` pairs at the capture
//!   rate (192 kHz today). Extension `.iq.f32`. Replayable by a
//!   future `efd-iq` file driver that just `mmap`s the file.
//! - **Audio** → raw `f32` mono PCM at the output rate (48 kHz).
//!   Extension `.pcm.f32`. Captured *before* Opus encode so no
//!   server-side decoder is required.
//!
//! Filenames are `YYYYMMDD-HHMMSS-<kind>.<ext>` under the directory
//! from `[recording] directory` in `config.toml`. If the client
//! supplies a `path` it is joined with the recordings root (after
//! stripping any `..` or leading `/`) so we can't write outside
//! the sandbox.
//!
//! Architecturally this is a single "rec-controller" task (one per
//! pipeline) driven by a `mpsc::Sender<RecCmd>`. Upstream handlers
//! enqueue Start/Stop; the controller spawns / cancels a per-
//! recording writer task.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use efd_proto::{AudioChunk, RecKind, RecordingStatus};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Commands sent from the WS upstream handlers to the controller.
#[derive(Debug, Clone)]
pub enum RecCmd {
    Start {
        kind: RecKind,
        /// Optional filename hint; controller picks a default when `None`.
        path: Option<String>,
    },
    Stop,
}

/// Controller handle stored in the pipeline. Clone the sender to
/// give each WS upstream handler its own submit point; the backing
/// task is one per pipeline.
#[derive(Clone)]
pub struct RecorderHandle {
    pub cmd_tx: mpsc::Sender<RecCmd>,
}

/// Sample rates the recorder needs to stamp on status pushes and
/// compute durations. Captured once at pipeline construction so
/// changes at runtime (not a thing today) don't affect existing
/// recordings mid-flight.
#[derive(Debug, Clone, Copy)]
pub struct RecorderRates {
    pub iq_sample_rate: u32,
    pub audio_sample_rate: u32,
}

/// Spawn the rec-controller.
///
/// Subscribes nothing on start; individual recordings subscribe their
/// own receiver of `iq_tx` / `pcm_tx` in-task so a disabled recorder
/// costs ~nothing.
pub fn spawn_controller(
    iq_tx: broadcast::Sender<Arc<efd_iq::IqBlock>>,
    pcm_tx: broadcast::Sender<Arc<Vec<f32>>>,
    status_tx: watch::Sender<RecordingStatus>,
    recordings_dir: PathBuf,
    rates: RecorderRates,
    cancel: CancellationToken,
) -> (RecorderHandle, JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<RecCmd>(8);
    let handle = tokio::spawn(run_controller(
        cmd_rx,
        iq_tx,
        pcm_tx,
        status_tx,
        recordings_dir,
        rates,
        cancel,
    ));
    (RecorderHandle { cmd_tx }, handle)
}

async fn run_controller(
    mut cmd_rx: mpsc::Receiver<RecCmd>,
    iq_tx: broadcast::Sender<Arc<efd_iq::IqBlock>>,
    pcm_tx: broadcast::Sender<Arc<Vec<f32>>>,
    status_tx: watch::Sender<RecordingStatus>,
    recordings_dir: PathBuf,
    rates: RecorderRates,
    cancel: CancellationToken,
) {
    let mut current: Option<ActiveRec> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(rec) = current.take() {
                    rec.stop().await;
                }
                break;
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    RecCmd::Start { kind, path } => {
                        if let Some(old) = current.take() {
                            debug!("REC: stopping previous recording before starting new");
                            old.stop().await;
                        }
                        match start(
                            kind,
                            path.as_deref(),
                            &iq_tx,
                            &pcm_tx,
                            &status_tx,
                            &recordings_dir,
                            rates,
                        )
                        .await
                        {
                            Ok(active) => {
                                info!(kind = ?kind, path = %active.path.display(), "REC started");
                                current = Some(active);
                            }
                            Err(e) => {
                                warn!("REC start failed: {e}");
                                let _ = status_tx.send(RecordingStatus {
                                    active: false,
                                    kind: None,
                                    path: None,
                                    bytes_written: 0,
                                    duration_s: None,
                                });
                            }
                        }
                    }
                    RecCmd::Stop => {
                        if let Some(rec) = current.take() {
                            info!(path = %rec.path.display(), "REC stopped");
                            rec.stop().await;
                        } else {
                            debug!("REC: Stop with no active recording");
                        }
                        let _ = status_tx.send(RecordingStatus {
                            active: false,
                            kind: None,
                            path: None,
                            bytes_written: 0,
                            duration_s: None,
                        });
                    }
                }
            }
        }
    }
    info!("rec-controller exiting");
}

struct ActiveRec {
    cancel: CancellationToken,
    join: JoinHandle<()>,
    path: PathBuf,
}

impl ActiveRec {
    async fn stop(self) {
        self.cancel.cancel();
        let _ = self.join.await;
    }
}

async fn start(
    kind: RecKind,
    client_path: Option<&str>,
    iq_tx: &broadcast::Sender<Arc<efd_iq::IqBlock>>,
    pcm_tx: &broadcast::Sender<Arc<Vec<f32>>>,
    status_tx: &watch::Sender<RecordingStatus>,
    recordings_dir: &Path,
    rates: RecorderRates,
) -> Result<ActiveRec, String> {
    tokio::fs::create_dir_all(recordings_dir)
        .await
        .map_err(|e| format!("create recordings dir {}: {e}", recordings_dir.display()))?;

    let ext = match kind {
        RecKind::Iq => "iq.f32",
        RecKind::Audio => "pcm.f32",
    };
    let path = resolve_path(client_path, recordings_dir, kind, ext)?;

    let file = File::create(&path)
        .await
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    let writer = BufWriter::with_capacity(64 * 1024, file);

    let cancel = CancellationToken::new();
    let status_tx = status_tx.clone();

    let join = match kind {
        RecKind::Iq => {
            let rx = iq_tx.subscribe();
            let c = cancel.clone();
            let p = path.clone();
            let sr = rates.iq_sample_rate;
            tokio::spawn(async move {
                run_iq_writer(rx, writer, p, sr, status_tx, c).await;
            })
        }
        RecKind::Audio => {
            let rx = pcm_tx.subscribe();
            let c = cancel.clone();
            let p = path.clone();
            let sr = rates.audio_sample_rate;
            tokio::spawn(async move {
                run_pcm_writer(rx, writer, p, sr, status_tx, c).await;
            })
        }
    };

    Ok(ActiveRec { cancel, join, path })
}

/// Resolve the output path.
///
/// - `None`: pick `{dir}/YYYYMMDD-HHMMSS-{kind}.{ext}`.
/// - `Some(p)`: strip leading `/` and any `..` components, then join
///   to `dir` so we can't escape the sandbox. If the caller didn't
///   include an extension, append the one for the kind.
fn resolve_path(
    client_path: Option<&str>,
    dir: &Path,
    kind: RecKind,
    ext: &str,
) -> Result<PathBuf, String> {
    match client_path {
        None => Ok(dir.join(default_filename(kind, ext))),
        Some(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Ok(dir.join(default_filename(kind, ext)));
            }
            let mut safe = PathBuf::new();
            for component in Path::new(raw).components() {
                use std::path::Component;
                match component {
                    Component::Normal(c) => safe.push(c),
                    // Silently drop anything that would escape — root,
                    // prefix, parent-dir. We don't error because the
                    // client might be oblivious; we just land the file
                    // at a safe basename.
                    Component::RootDir | Component::Prefix(_) | Component::ParentDir => {}
                    Component::CurDir => {}
                }
            }
            if safe.as_os_str().is_empty() {
                return Ok(dir.join(default_filename(kind, ext)));
            }
            // Append default extension if the client didn't include one.
            if safe.extension().is_none() {
                safe.set_extension(ext);
            }
            Ok(dir.join(safe))
        }
    }
}

fn default_filename(kind: RecKind, ext: &str) -> String {
    let tag = match kind {
        RecKind::Iq => "iq",
        RecKind::Audio => "audio",
    };
    format!("{}-{tag}.{ext}", timestamp_now())
}

fn timestamp_now() -> String {
    // Plain `YYYYMMDD-HHMMSS` in UTC — good enough for a filename,
    // avoids needing chrono. SystemTime → naive UTC manually.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Epoch-based breakdown into Y/M/D/H/M/S. Good through 2100+.
    let (y, mo, d, h, mi, s) = break_down_utc(now);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Convert seconds-since-epoch to (year, month, day, hour, minute, second)
/// in UTC. Avoids a chrono dep for a one-off filename stamp. Not leap-
/// second-aware; fine for human-readable recording names.
fn break_down_utc(epoch_s: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = epoch_s / 86_400;
    let rem = epoch_s % 86_400;
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    // Days since 1970-01-01. Walk year by year; fine through 2300-ish.
    let mut year = 1970u32;
    let mut day_of_year = days as u32;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if day_of_year < days_in_year {
            break;
        }
        day_of_year -= days_in_year;
        year += 1;
    }
    let month_days = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    let mut day = day_of_year;
    for (i, len) in month_days.iter().enumerate() {
        if day < *len {
            month = (i + 1) as u32;
            break;
        }
        day -= len;
    }
    let day = day + 1;
    (year, month, day, h, mi, s)
}

fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

async fn run_iq_writer(
    mut rx: broadcast::Receiver<Arc<efd_iq::IqBlock>>,
    mut writer: BufWriter<File>,
    path: PathBuf,
    sample_rate: u32,
    status_tx: watch::Sender<RecordingStatus>,
    cancel: CancellationToken,
) {
    let mut bytes: u64 = 0;
    let mut sample_count: u64 = 0;
    let mut last_status = tokio::time::Instant::now();

    let _ = status_tx.send(RecordingStatus {
        active: true,
        kind: Some(RecKind::Iq),
        path: Some(path.display().to_string()),
        bytes_written: 0,
        duration_s: Some(0.0),
    });

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            r = rx.recv() => {
                let block = match r {
                    Ok(b) => b,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "REC IQ: broadcast lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                // Interleaved [I,Q] f32 pairs, little-endian native on
                // our target (Pi CM5 is aarch64-le; x86 dev is also le).
                let bytes_slice = cast_iq_to_bytes(&block.samples);
                if let Err(e) = writer.write_all(bytes_slice).await {
                    error!("REC IQ write error: {e}");
                    break;
                }
                bytes = bytes.saturating_add(bytes_slice.len() as u64);
                sample_count = sample_count.saturating_add(block.samples.len() as u64);
                maybe_push_status(
                    &mut last_status,
                    &status_tx,
                    RecKind::Iq,
                    &path,
                    bytes,
                    sample_count,
                    Some(sample_rate),
                );
            }
        }
    }

    if let Err(e) = writer.flush().await {
        warn!("REC IQ final flush: {e}");
    }
    let duration_s = if sample_rate > 0 {
        Some(sample_count as f64 / sample_rate as f64)
    } else {
        None
    };
    info!(bytes, samples = sample_count, ?duration_s, "REC IQ closed");
}

async fn run_pcm_writer(
    mut rx: broadcast::Receiver<Arc<Vec<f32>>>,
    mut writer: BufWriter<File>,
    path: PathBuf,
    sample_rate: u32,
    status_tx: watch::Sender<RecordingStatus>,
    cancel: CancellationToken,
) {
    let mut bytes: u64 = 0;
    let mut sample_count: u64 = 0;
    let mut last_status = tokio::time::Instant::now();

    let _ = status_tx.send(RecordingStatus {
        active: true,
        kind: Some(RecKind::Audio),
        path: Some(path.display().to_string()),
        bytes_written: 0,
        duration_s: Some(0.0),
    });

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            r = rx.recv() => {
                let chunk = match r {
                    Ok(b) => b,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "REC PCM: broadcast lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let bytes_slice = cast_f32_to_bytes(&chunk);
                if let Err(e) = writer.write_all(bytes_slice).await {
                    error!("REC PCM write error: {e}");
                    break;
                }
                bytes = bytes.saturating_add(bytes_slice.len() as u64);
                sample_count = sample_count.saturating_add(chunk.len() as u64);
                maybe_push_status(
                    &mut last_status,
                    &status_tx,
                    RecKind::Audio,
                    &path,
                    bytes,
                    sample_count,
                    Some(sample_rate),
                );
            }
        }
    }

    if let Err(e) = writer.flush().await {
        warn!("REC PCM final flush: {e}");
    }
    let duration_s = sample_count as f64 / sample_rate as f64;
    info!(bytes, samples = sample_count, duration_s, "REC PCM closed");
}

/// Push a `RecordingStatus` at most once per second so clients see
/// progress without spamming the WS channel.
fn maybe_push_status(
    last: &mut tokio::time::Instant,
    status_tx: &watch::Sender<RecordingStatus>,
    kind: RecKind,
    path: &Path,
    bytes: u64,
    sample_count: u64,
    sample_rate: Option<u32>,
) {
    const STATUS_INTERVAL: Duration = Duration::from_secs(1);
    if last.elapsed() < STATUS_INTERVAL {
        return;
    }
    *last = tokio::time::Instant::now();
    let duration_s = sample_rate
        .filter(|sr| *sr > 0)
        .map(|sr| sample_count as f64 / sr as f64);
    let _ = status_tx.send(RecordingStatus {
        active: true,
        kind: Some(kind),
        path: Some(path.display().to_string()),
        bytes_written: bytes,
        duration_s,
    });
}

// --- byte casts ---

/// Reinterpret `&[[f32; 2]]` as a byte slice. Sound because
/// `[f32; 2]` is `#[repr(Rust)]` but in practice has the layout of
/// `[f32, f32]` with no padding, and `f32` has no invalid bit
/// patterns for writing.
fn cast_iq_to_bytes(samples: &[[f32; 2]]) -> &[u8] {
    // SAFETY: the layout of `[[f32; 2]]` is dense `f32` with no
    // padding; we're converting to `&[u8]` for a write-only purpose,
    // so every bit pattern is valid.
    unsafe {
        std::slice::from_raw_parts(
            samples.as_ptr() as *const u8,
            std::mem::size_of_val(samples),
        )
    }
}

fn cast_f32_to_bytes(samples: &[f32]) -> &[u8] {
    // SAFETY: same rationale — `f32` is `Copy` with no padding; we're
    // reading the bit pattern to write to disk.
    unsafe {
        std::slice::from_raw_parts(
            samples.as_ptr() as *const u8,
            std::mem::size_of_val(samples),
        )
    }
}

// --- compatibility shim: AudioChunk subscriber (unused today) ---
//
// Kept as a hint for how an Opus-decoding path would plug in later
// (subscribe to AudioChunk, run through audiopus, write WAV).
// The current PCM path is simpler and faster.
#[allow(dead_code)]
pub fn spawn_opus_subscriber(
    _audio_tx: broadcast::Sender<AudioChunk>,
) -> JoinHandle<()> {
    tokio::spawn(async {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_numbers() {
        // 0 = 1970-01-01 00:00:00
        let (y, mo, d, h, mi, s) = break_down_utc(0);
        assert_eq!((y, mo, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
        // 2020-01-01 00:00:00 = 1577836800
        let (y, mo, d, _, _, _) = break_down_utc(1577836800);
        assert_eq!((y, mo, d), (2020, 1, 1));
        // 2024-02-29 (leap) 12:34:56 = 1709210096
        let (y, mo, d, h, mi, s) = break_down_utc(1709210096);
        assert_eq!((y, mo, d, h, mi, s), (2024, 2, 29, 12, 34, 56));
    }

    #[test]
    fn is_leap_handles_centuries() {
        assert!(is_leap(2000));
        assert!(!is_leap(1900));
        assert!(is_leap(2024));
        assert!(!is_leap(2023));
    }

    #[test]
    fn resolve_path_defaults_when_none() {
        let p = resolve_path(None, Path::new("/recs"), RecKind::Iq, "iq.f32").unwrap();
        assert!(p.starts_with("/recs"));
        let s = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(s.ends_with(".iq.f32"), "got {s}");
        assert!(s.contains("iq"));
    }

    #[test]
    fn resolve_path_sandboxes_client_input() {
        // Absolute root gets stripped, parent-dir components dropped.
        let p = resolve_path(
            Some("/etc/passwd"),
            Path::new("/recs"),
            RecKind::Iq,
            "iq.f32",
        )
        .unwrap();
        assert_eq!(p, Path::new("/recs/etc/passwd.iq.f32"));

        let p = resolve_path(
            Some("../../etc/passwd"),
            Path::new("/recs"),
            RecKind::Iq,
            "iq.f32",
        )
        .unwrap();
        assert_eq!(p, Path::new("/recs/etc/passwd.iq.f32"));
    }

    #[test]
    fn resolve_path_keeps_client_extension() {
        let p = resolve_path(
            Some("mytest.raw"),
            Path::new("/recs"),
            RecKind::Iq,
            "iq.f32",
        )
        .unwrap();
        assert_eq!(p, Path::new("/recs/mytest.raw"));
    }
}
