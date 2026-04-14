//! File-backed audio source — reads a WAV/FLAC recording from disk,
//! decodes to mono f32 at a caller-chosen sample rate, and emits
//! `PcmBlock` chunks paced in real time. Fits the "first-class file
//! source" role in `docs/CM5-sdr-backend-pipeline.drawio`.
//!
//! Phase 2 scope: no resampling. The source file's rate must match the
//! configured rate (typically 48 000 Hz). Stereo files are downmixed
//! to mono by averaging channels. On EOF the task exits cleanly.
//!
//! The DRM file-test shortcut (`start_drm_file_test` in the server)
//! keeps using DREAM's own `-f` flag — this module doesn't replace it.

use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecType, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::error::AudioError;
use crate::PcmBlock;

/// Chunk size for outgoing `PcmBlock`s. Matches the 960-sample frame
/// the USB RX task uses so downstream consumers see the same shape.
const FRAME_SAMPLES: usize = 960;

#[derive(Debug, Clone)]
pub struct FileSourceConfig {
    /// Path to a WAV or FLAC file. Extension-probed by symphonia.
    pub path: PathBuf,
    /// Expected sample rate. File rate must match (no resampling in
    /// Phase 2); mismatched rates return `AudioError::FileConfig`.
    pub sample_rate: u32,
}

/// Spawn a blocking task that streams the file in real time.
/// The returned handle resolves when the file has played through or
/// the cancel token fires.
pub fn spawn_file_source_task(
    config: FileSourceConfig,
    audio_tx: mpsc::Sender<PcmBlock>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), AudioError>> {
    tokio::task::spawn_blocking(move || run_file_source(config, audio_tx, cancel))
}

fn run_file_source(
    config: FileSourceConfig,
    audio_tx: mpsc::Sender<PcmBlock>,
    cancel: CancellationToken,
) -> Result<(), AudioError> {
    let file = File::open(&config.path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = config.path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let probed = get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| AudioError::FileConfig("no decodable track in file".into()))?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();

    let file_rate = codec_params
        .sample_rate
        .ok_or_else(|| AudioError::FileConfig("file sample rate unknown".into()))?;
    if file_rate != config.sample_rate {
        return Err(AudioError::FileConfig(format!(
            "file rate {file_rate} Hz does not match configured rate {} Hz (no resampling yet)",
            config.sample_rate
        )));
    }
    let channels = codec_params
        .channels
        .map(|c| c.count())
        .ok_or_else(|| AudioError::FileConfig("file channel layout unknown".into()))?;

    let mut decoder = get_codecs().make(&codec_params, &DecoderOptions::default())?;

    info!(
        path = %config.path.display(),
        rate = file_rate,
        channels,
        codec = codec_type_name(codec_params.codec),
        "file source opened"
    );

    // Reused across packets to avoid per-packet allocation.
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    // Mono f32 accumulator; we emit in FRAME_SAMPLES chunks.
    let mut mono_pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);
    let mut samples_emitted: u64 = 0;
    let start = Instant::now();

    loop {
        if cancel.is_cancelled() {
            return Err(AudioError::Cancelled);
        }

        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                // Flush any partial frame then exit cleanly.
                if !mono_pending.is_empty() {
                    let _ = audio_tx.blocking_send(PcmBlock {
                        samples: std::mem::take(&mut mono_pending),
                    });
                }
                info!("file source reached EOF");
                return Ok(());
            }
            Err(e) => return Err(AudioError::Decode(e)),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(msg)) => {
                warn!("file source decode error, skipping: {msg}");
                continue;
            }
            Err(e) => return Err(AudioError::Decode(e)),
        };

        // Materialise to interleaved f32 once we know the spec.
        let buf = sample_buf.get_or_insert_with(|| {
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec())
        });
        buf.copy_interleaved_ref(decoded);
        let interleaved = buf.samples();

        // Downmix to mono by averaging channels.
        mono_pending.extend(
            interleaved
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32),
        );

        // Emit in FRAME_SAMPLES-sized chunks, paced to real time.
        while mono_pending.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = mono_pending.drain(..FRAME_SAMPLES).collect();
            samples_emitted += FRAME_SAMPLES as u64;

            let expected = Duration::from_secs_f64(
                samples_emitted as f64 / config.sample_rate as f64,
            );
            let actual = start.elapsed();
            if expected > actual {
                std::thread::sleep(expected - actual);
            }

            if audio_tx.blocking_send(PcmBlock { samples: frame }).is_err() {
                return Err(AudioError::ChannelClosed);
            }
        }
    }
}

fn codec_type_name(codec: CodecType) -> &'static str {
    use symphonia::core::codecs::{CODEC_TYPE_FLAC, CODEC_TYPE_PCM_S16LE, CODEC_TYPE_PCM_S24LE,
        CODEC_TYPE_PCM_S32LE, CODEC_TYPE_PCM_F32LE, CODEC_TYPE_PCM_F64LE};
    match codec {
        CODEC_TYPE_FLAC => "FLAC",
        CODEC_TYPE_PCM_S16LE => "PCM s16le",
        CODEC_TYPE_PCM_S24LE => "PCM s24le",
        CODEC_TYPE_PCM_S32LE => "PCM s32le",
        CODEC_TYPE_PCM_F32LE => "PCM f32le",
        CODEC_TYPE_PCM_F64LE => "PCM f64le",
        _ => "unknown",
    }
}
