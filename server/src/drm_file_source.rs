//! Audio-IF file source for the DRM file-test pipeline.
//!
//! Reads a mono 48 kHz WAV or FLAC recording and publishes it onto a
//! `broadcast<AudioBlock>` at wall-clock rate, mimicking the format the
//! wideband-SSB demod produces under `Mode::DRM`. The DRM bridge
//! downstream consumes the same channel shape either way — no changes
//! to `efd-dsp` needed.
//!
//! Triggered by `EFD_DRM_FILE_TEST=/path/to/file.flac` on server start.
//! Loops-free: on EOF, the task fires the top-level cancel token and
//! returns, which tears down axum gracefully.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use efd_dsp::AudioBlock;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// 20 ms at 48 kHz — matches the block size the demod and DRM bridge
/// use elsewhere so downstream backpressure behaviour is consistent.
const FRAMES_PER_BLOCK: usize = 960;

/// Spawn the file-source task.
pub fn spawn(
    path: PathBuf,
    drm_if_tx: broadcast::Sender<AudioBlock>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || run(path, drm_if_tx, cancel))
}

fn run(
    path: PathBuf,
    drm_if_tx: broadcast::Sender<AudioBlock>,
    cancel: CancellationToken,
) {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    let result = match ext.as_str() {
        "flac" | "fla" => stream_flac(&path, &drm_if_tx, &cancel),
        "wav" => stream_wav(&path, &drm_if_tx, &cancel),
        other => Err(format!(
            "unsupported file extension {other:?} (expected .flac, .fla, or .wav)"
        )),
    };

    match result {
        Ok(()) => info!(file = %path.display(), "drm file source: EOF"),
        Err(e) => error!(file = %path.display(), "drm file source error: {e}"),
    }

    // Either way the stream is done — tell the rest of the server to
    // shut down so the process exits cleanly.
    cancel.cancel();
}

fn stream_flac(
    path: &std::path::Path,
    tx: &broadcast::Sender<AudioBlock>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let mut reader = claxon::FlacReader::open(path)
        .map_err(|e| format!("open flac {}: {e}", path.display()))?;
    let info = reader.streaminfo();
    let sample_rate = info.sample_rate;
    let channels = info.channels;
    let bits = info.bits_per_sample;
    info!(
        file = %path.display(),
        sample_rate, channels, bits, "drm file source: opened FLAC"
    );

    let scale = 1.0f32 / (1i64 << (bits - 1)) as f32;
    let block_period = block_period(sample_rate);

    let mut block = Vec::with_capacity(FRAMES_PER_BLOCK);
    let started = Instant::now();
    let mut samples_written: u64 = 0;
    let mut blocks = reader.blocks();
    let mut buf = Vec::new();

    loop {
        let frame = blocks
            .read_next_or_eof(std::mem::take(&mut buf))
            .map_err(|e| format!("flac decode: {e}"))?;
        let Some(frame) = frame else { break };
        let frame_channels = frame.channels() as usize;
        let frame_len = frame.len() / frame_channels as u32;
        for i in 0..frame_len as usize {
            // Downmix to mono: average across channels (typically 1).
            let mut acc = 0i64;
            for ch in 0..frame_channels {
                acc += frame.sample(ch as u32, i as u32) as i64;
            }
            let mono = (acc as f32 / frame_channels as f32) * scale;
            block.push(mono);
            if block.len() == FRAMES_PER_BLOCK {
                if push_and_pace(
                    tx,
                    &mut block,
                    sample_rate,
                    started,
                    &mut samples_written,
                    block_period,
                    cancel,
                )? {
                    return Ok(());
                }
            }
        }
        buf = frame.into_buffer();
    }

    // Flush partial trailing block.
    if !block.is_empty() {
        let _ = push_block(tx, &mut block, sample_rate, started);
    }
    Ok(())
}

fn stream_wav(
    path: &std::path::Path,
    tx: &broadcast::Sender<AudioBlock>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("open wav {}: {e}", path.display()))?;
    let mut reader = hound::WavReader::new(BufReader::new(file))
        .map_err(|e| format!("parse wav {}: {e}", path.display()))?;
    let spec = reader.spec();
    info!(
        file = %path.display(),
        sample_rate = spec.sample_rate,
        channels = spec.channels,
        bits = spec.bits_per_sample,
        fmt = ?spec.sample_format,
        "drm file source: opened WAV"
    );

    let block_period = block_period(spec.sample_rate);
    let mut block = Vec::with_capacity(FRAMES_PER_BLOCK);
    let started = Instant::now();
    let mut samples_written: u64 = 0;

    // Iterate interleaved samples; downmix to mono by averaging channels.
    // The demod upstream always produces mono, so the DRM bridge expects
    // mono here too.
    let channels = spec.channels as usize;
    let scale = match spec.sample_format {
        hound::SampleFormat::Int => 1.0f32 / (1i64 << (spec.bits_per_sample - 1)) as f32,
        hound::SampleFormat::Float => 1.0,
    };

    let mut frame_acc = 0.0f32;
    let mut in_frame: usize = 0;

    let res: Result<(), hound::Error> = (|| {
        match spec.sample_format {
            hound::SampleFormat::Int => {
                for s in reader.samples::<i32>() {
                    let v = s? as f32 * scale;
                    frame_acc += v;
                    in_frame += 1;
                    if in_frame == channels {
                        let mono = frame_acc / channels as f32;
                        frame_acc = 0.0;
                        in_frame = 0;
                        block.push(mono);
                        if block.len() == FRAMES_PER_BLOCK
                            && push_and_pace(
                                tx,
                                &mut block,
                                spec.sample_rate,
                                started,
                                &mut samples_written,
                                block_period,
                                cancel,
                            )
                            .map_err(|e| hound::Error::IoError(std::io::Error::other(e)))?
                        {
                            return Ok(());
                        }
                    }
                }
            }
            hound::SampleFormat::Float => {
                for s in reader.samples::<f32>() {
                    let v = s?;
                    frame_acc += v;
                    in_frame += 1;
                    if in_frame == channels {
                        let mono = frame_acc / channels as f32;
                        frame_acc = 0.0;
                        in_frame = 0;
                        block.push(mono);
                        if block.len() == FRAMES_PER_BLOCK
                            && push_and_pace(
                                tx,
                                &mut block,
                                spec.sample_rate,
                                started,
                                &mut samples_written,
                                block_period,
                                cancel,
                            )
                            .map_err(|e| hound::Error::IoError(std::io::Error::other(e)))?
                        {
                            return Ok(());
                        }
                    }
                }
            }
        }
        Ok(())
    })();
    res.map_err(|e| format!("wav decode: {e}"))?;

    if !block.is_empty() {
        let _ = push_block(tx, &mut block, spec.sample_rate, started);
    }
    Ok(())
}

fn block_period(sample_rate: u32) -> Duration {
    Duration::from_secs_f64(FRAMES_PER_BLOCK as f64 / sample_rate as f64)
}

/// Returns `Ok(true)` if the caller should stop (cancellation fired).
fn push_and_pace(
    tx: &broadcast::Sender<AudioBlock>,
    block: &mut Vec<f32>,
    sample_rate: u32,
    started: Instant,
    samples_written: &mut u64,
    block_period: Duration,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    if cancel.is_cancelled() {
        return Ok(true);
    }
    push_block(tx, block, sample_rate, started);
    *samples_written += FRAMES_PER_BLOCK as u64;
    // Pace to the file's real-time playout: sleep until (samples_written /
    // sample_rate) has elapsed since we started. Keeps DREAM's input
    // buffer from filling in bursts and lets the rest of the pipeline
    // run at normal real-time cadence.
    let target = Duration::from_secs_f64(*samples_written as f64 / sample_rate as f64);
    let elapsed = started.elapsed();
    if target > elapsed {
        let dur = target - elapsed;
        // Not tokio::sleep — we're in spawn_blocking. std::thread::sleep
        // is cheap and precise enough (ms resolution).
        std::thread::sleep(dur);
    } else if elapsed - target > block_period * 4 {
        warn!(
            behind_ms = (elapsed - target).as_millis() as u64,
            "drm file source falling behind wall clock"
        );
    }
    Ok(false)
}

fn push_block(
    tx: &broadcast::Sender<AudioBlock>,
    block: &mut Vec<f32>,
    sample_rate: u32,
    started: Instant,
) {
    let samples = std::mem::take(block);
    let blk = AudioBlock {
        samples,
        sample_rate,
        timestamp_us: started.elapsed().as_micros() as u64,
    };
    // broadcast::send returns Err only when every receiver has been
    // dropped — that's normal between DRM bridge respawns, ignore it.
    match tx.send(blk) {
        Ok(_n) => {}
        Err(_) => debug!("drm file source: no active receivers"),
    }
}
