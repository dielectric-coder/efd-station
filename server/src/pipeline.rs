use std::sync::Arc;
use std::time::Duration;

use efd_proto::{AudioChunk, AudioSource, CatCommand, FftBins, RadioState, TxAudio};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::config::Config;

/// Handles returned by the pipeline — used by WebSocket handlers to
/// subscribe to data and send commands.
pub struct Pipeline {
    // -- broadcast senders (clone to subscribe) --
    pub fft_tx: broadcast::Sender<Arc<FftBins>>,
    pub state_tx: broadcast::Sender<RadioState>,
    pub audio_tx: broadcast::Sender<AudioChunk>,

    // -- mpsc senders (clone for each WS client) --
    pub cat_tx: mpsc::Sender<CatCommand>,
    pub tx_audio_tx: mpsc::Sender<TxAudio>,

    /// Client demod mode override — None = use radio's mode, Some = SDR override.
    pub demod_mode_tx: watch::Sender<Option<efd_proto::Mode>>,

    /// Audio source selection: SoftwareDemod or RadioUsb.
    pub audio_source_tx: watch::Sender<AudioSource>,

    pub(crate) cancel: CancellationToken,
    tasks: Vec<(&'static str, JoinHandle<()>)>,
}

impl Pipeline {
    /// Create all channels, spawn all tasks.
    pub fn start(config: &Config) -> Self {
        let cancel = CancellationToken::new();

        // -- broadcast channels (fan-out) --
        // efd-dsp now uses efd_iq::IqBlock directly — no forwarder needed
        let (iq_tx, _) = broadcast::channel::<Arc<efd_iq::IqBlock>>(16);
        let (fft_tx, _) = broadcast::channel::<Arc<FftBins>>(8);
        let (state_tx, _) = broadcast::channel::<RadioState>(16);
        let (audio_tx, _) = broadcast::channel::<AudioChunk>(32);

        // -- mpsc channels (single consumer) --
        let (cat_tx, cat_rx) = mpsc::channel::<CatCommand>(64);
        let (tx_audio_tx, tx_audio_rx) = mpsc::channel::<TxAudio>(64);
        let (demod_audio_tx, demod_audio_rx) = mpsc::channel::<efd_dsp::AudioBlock>(64);

        let mut tasks: Vec<(&'static str, JoinHandle<()>)> = Vec::new();

        // --- IQ capture task ---
        let (iq_center_tx, iq_center_rx) = tokio::sync::watch::channel(0u64);
        {
            let iq_cfg = efd_iq::IqConfig {
                vendor_id: config.usb.vendor_id,
                product_id: config.usb.product_id,
            };
            let tx = iq_tx.clone();
            let c = cancel.clone();
            let handle = efd_iq::spawn_iq_capture(iq_cfg, tx, iq_center_tx, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("IQ capture exited cleanly"),
                    Ok(Err(e)) => error!("IQ capture error: {e}"),
                    Err(e) => error!("IQ capture panicked: {e}"),
                }
            });
            tasks.push(("iq_capture", handle));
        }

        // --- FFT task ---
        {
            let fft_cfg = efd_dsp::FftConfig {
                fft_size: config.dsp.fft_size,
                averaging: config.dsp.fft_averaging,
                center_freq_hz: 0, // updated from RadioState by client
                span_hz: config.dsp.sample_rate,
                ref_level_db: -20.0,
            };
            let iq_rx = iq_tx.subscribe();
            let fft_tx2 = fft_tx.clone();
            let c = cancel.clone();
            let handle = efd_dsp::spawn_fft_task(iq_rx, fft_tx2, fft_cfg, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("FFT task exited cleanly"),
                    Ok(Err(e)) => error!("FFT task error: {e}"),
                    Err(e) => error!("FFT task panicked: {e}"),
                }
            });
            tasks.push(("fft", handle));
        }

        // --- Demod mode override from client (SDR mode) ---
        let (demod_mode_tx, demod_mode_rx) =
            tokio::sync::watch::channel::<Option<efd_proto::Mode>>(None);

        // --- Demod task ---
        let (demod_tuning_tx, demod_tuning_rx) =
            tokio::sync::watch::channel(efd_dsp::DemodTuning::default());
        {
            let iq_rx = iq_tx.subscribe();
            let demod_cfg = efd_dsp::DemodConfig {
                input_rate: config.dsp.sample_rate,
                output_rate: config.audio.sample_rate,
                mode: efd_proto::Mode::USB,
            };
            let dtx = demod_audio_tx;
            let c = cancel.clone();
            let handle = efd_dsp::spawn_demod_task(iq_rx, dtx, demod_cfg, demod_tuning_rx, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("demod task exited cleanly"),
                    Ok(Err(e)) => error!("demod task error: {e}"),
                    Err(e) => error!("demod task panicked: {e}"),
                }
            });
            tasks.push(("demod", handle));
        }

        // --- USB RX audio capture (radio's hardware demod output) ---
        // Only spawn if rx_device is configured — an unconfigured device would
        // block on ALSA open/read and prevent clean shutdown.
        let (usb_rx_tx, usb_rx_rx) = mpsc::channel::<efd_audio::PcmBlock>(64);
        if !config.audio.rx_device.is_empty() {
            let usb_rx_cfg = efd_audio::UsbRxConfig {
                device: config.audio.rx_device.clone(),
                sample_rate: config.audio.sample_rate,
            };
            let c = cancel.clone();
            let handle = efd_audio::spawn_usb_rx_task(usb_rx_cfg, usb_rx_tx, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("USB RX capture exited cleanly"),
                    Ok(Err(e)) => error!("USB RX capture error: {e}"),
                    Err(e) => error!("USB RX capture panicked: {e}"),
                }
            });
            tasks.push(("usb_rx", handle));
        } else {
            drop(usb_rx_tx); // no producer — mux will see closed channel immediately
            info!("USB RX audio disabled (rx_device not configured)");
        }

        // --- Audio source mux → Opus encoder → broadcast<AudioChunk> ---
        let (audio_source_tx, audio_source_rx) = watch::channel(AudioSource::SoftwareDemod);
        {
            let atx = audio_tx.clone();
            let c = cancel.clone();
            let handle = tokio::spawn(async move {
                encode_audio_mux(demod_audio_rx, usb_rx_rx, audio_source_rx, atx, c).await;
            });
            tasks.push(("opus_encoder", handle));
        }

        // --- ALSA playback task ---
        {
            let alsa_cfg = efd_audio::AlsaConfig {
                device: config.audio.alsa_device.clone(),
                sample_rate: config.audio.sample_rate,
                latency_ms: 50,
            };
            let (alsa_tx, alsa_rx) = mpsc::channel::<efd_audio::PcmBlock>(64);
            let audio_rx_for_alsa = audio_tx.subscribe();
            let c = cancel.clone();
            let alsa_bridge = tokio::spawn(async move {
                decode_audio_for_alsa(audio_rx_for_alsa, alsa_tx, c).await;
            });
            tasks.push(("alsa_bridge", alsa_bridge));

            let c2 = cancel.clone();
            let handle = efd_audio::spawn_alsa_task(alsa_cfg, alsa_rx, c2);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("ALSA task exited cleanly"),
                    Ok(Err(e)) => error!("ALSA task error: {e}"),
                    Err(e) => error!("ALSA task panicked: {e}"),
                }
            });
            tasks.push(("alsa", handle));
        }

        // --- USB TX audio task ---
        {
            let usb_tx_cfg = efd_audio::UsbTxConfig {
                device: config.audio.tx_device.clone(),
                sample_rate: config.audio.sample_rate,
            };
            let c = cancel.clone();
            let handle = efd_audio::spawn_usb_tx_task(usb_tx_cfg, tx_audio_rx, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("USB TX task exited cleanly"),
                    Ok(Err(e)) => error!("USB TX task error: {e}"),
                    Err(e) => error!("USB TX task panicked: {e}"),
                }
            });
            tasks.push(("usb_tx", handle));
        }

        // --- CAT tasks (direct serial, no rigctld) ---
        {
            let cat_cfg = efd_cat::CatConfig {
                serial_device: config.cat.serial_device.clone(),
                poll_interval: Duration::from_millis(config.cat.poll_interval_ms),
            };
            let st_tx = state_tx.clone();
            let c = cancel.clone();
            let (poll_h, cmd_h) = efd_cat::spawn_cat_tasks(cat_cfg, st_tx, cat_rx, c);

            let poll_w = tokio::spawn(async move {
                match poll_h.await {
                    Ok(Ok(())) => info!("CAT poll exited cleanly"),
                    Ok(Err(e)) => error!("CAT poll error: {e}"),
                    Err(e) => error!("CAT poll panicked: {e}"),
                }
            });
            tasks.push(("cat_poll", poll_w));

            let cmd_w = tokio::spawn(async move {
                match cmd_h.await {
                    Ok(Ok(())) => info!("CAT cmd exited cleanly"),
                    Ok(Err(e)) => error!("CAT cmd error: {e}"),
                    Err(e) => error!("CAT cmd panicked: {e}"),
                }
            });
            tasks.push(("cat_cmd", cmd_w));
        }

        // --- Tuning forwarder: RadioState + IQ center + mode override → demod tuning ---
        {
            let mut state_rx = state_tx.subscribe();
            let tuning_tx = demod_tuning_tx;
            let iq_center = iq_center_rx;
            let mode_override = demod_mode_rx;
            let c = cancel.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        result = state_rx.recv() => {
                            match result {
                                Ok(state) => {
                                    let iq_center_hz = *iq_center.borrow();
                                    let vfo_offset = if iq_center_hz > 0 {
                                        state.freq_hz as f64 - iq_center_hz as f64
                                    } else {
                                        0.0
                                    };
                                    let filter_bw = parse_filter_bw(&state.filter_bw);
                                    // Use client override if set (SDR mode),
                                    // otherwise use radio's reported mode (Monitor mode).
                                    let mode = mode_override.borrow()
                                        .unwrap_or(state.mode);
                                    // No SSB offset here — the channel filter in
                                    // efd-dsp uses a complex bandpass to select
                                    // only the desired sideband around DC.
                                    let _ = tuning_tx.send(efd_dsp::DemodTuning {
                                        mode,
                                        vfo_offset_hz: vfo_offset,
                                        filter_bw_hz: filter_bw,
                                    });
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }
                }
            });
            tasks.push(("tuning_fwd", handle));
        }

        info!(tasks = tasks.len(), "pipeline started");

        Self {
            fft_tx,
            state_tx,
            audio_tx,
            cat_tx,
            tx_audio_tx,
            demod_mode_tx,
            audio_source_tx,
            cancel,
            tasks,
        }
    }

    /// Graceful shutdown — cancel all tasks and await them.
    pub async fn shutdown(self) {
        info!("pipeline shutting down");
        self.cancel.cancel();
        for (name, handle) in self.tasks {
            if let Err(e) = handle.await {
                error!(task = name, "join error: {e}");
            }
        }
        info!("pipeline shutdown complete");
    }
}

/// Mux between demod and USB RX audio, encode to Opus, broadcast.
///
/// Both sources are always drained to prevent backpressure on the idle one.
/// Only the active source (selected by `source_rx`) feeds the Opus encoder.
async fn encode_audio_mux(
    mut demod_rx: mpsc::Receiver<efd_dsp::AudioBlock>,
    mut usb_rx: mpsc::Receiver<efd_audio::PcmBlock>,
    mut source_rx: watch::Receiver<AudioSource>,
    tx: broadcast::Sender<AudioChunk>,
    cancel: CancellationToken,
) {
    let mut encoder = match efd_audio::OpusEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            error!("Opus encoder init failed: {e}");
            return;
        }
    };
    let mut seq: u32 = 0;
    let mut frame_buf: Vec<f32> = Vec::with_capacity(efd_audio::OPUS_FRAME_SIZE);
    let mut demod_alive = true;
    let mut usb_alive = true;

    loop {
        let source = *source_rx.borrow();
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = source_rx.changed() => {
                // Source changed — flush partial frame to avoid mixing audio.
                frame_buf.clear();
                continue;
            }
            block = demod_rx.recv(), if demod_alive => {
                let Some(block) = block else {
                    demod_alive = false;
                    tracing::warn!("demod audio channel closed");
                    continue;
                };
                if source != AudioSource::SoftwareDemod {
                    continue;
                }
                encode_samples(&block.samples, &mut frame_buf, &mut encoder, &mut seq, &tx);
            }
            block = usb_rx.recv(), if usb_alive => {
                let Some(block) = block else {
                    usb_alive = false;
                    tracing::warn!("USB RX audio channel closed");
                    continue;
                };
                if source != AudioSource::RadioUsb {
                    continue;
                }
                encode_samples(&block.samples, &mut frame_buf, &mut encoder, &mut seq, &tx);
            }
        }
    }
}

/// Accumulate samples into Opus frames and broadcast encoded chunks.
fn encode_samples(
    samples: &[f32],
    frame_buf: &mut Vec<f32>,
    encoder: &mut efd_audio::OpusEncoder,
    seq: &mut u32,
    tx: &broadcast::Sender<AudioChunk>,
) {
    for &sample in samples {
        frame_buf.push(sample);
        if frame_buf.len() == efd_audio::OPUS_FRAME_SIZE {
            match encoder.encode_float(frame_buf) {
                Ok(opus_data) => {
                    let chunk = AudioChunk { opus_data, seq: *seq };
                    *seq = seq.wrapping_add(1);
                    let _ = tx.send(chunk);
                }
                Err(e) => {
                    tracing::warn!("Opus encode error: {e}");
                }
            }
            frame_buf.clear();
        }
    }
}

/// Parse FDM-DUO filter bandwidth string to Hz.
fn parse_filter_bw(bw: &str) -> f64 {
    let s = bw.trim();
    match s {
        "Narrow" => 6_000.0,
        "Wide" => 15_000.0,
        "Data" => 9_000.0,
        _ => {
            let s = s.strip_prefix('D').unwrap_or(s);
            let s = s.split('&').next().unwrap_or(s);
            if let Some(num) = s.strip_suffix('k') {
                num.parse::<f64>().unwrap_or(3000.0) * 1000.0
            } else {
                s.parse::<f64>().unwrap_or(3000.0)
            }
        }
    }
}

/// Decode Opus AudioChunks back to PCM for ALSA local playback.
async fn decode_audio_for_alsa(
    mut rx: broadcast::Receiver<AudioChunk>,
    tx: mpsc::Sender<efd_audio::PcmBlock>,
    cancel: CancellationToken,
) {
    let mut decoder = match efd_audio::OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            error!("Opus decoder init failed: {e}");
            return;
        }
    };

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = rx.recv() => {
                match result {
                    Ok(chunk) => {
                        match decoder.decode_float(&chunk.opus_data) {
                            Ok(pcm) => {
                                let block = efd_audio::PcmBlock { samples: pcm };
                                if tx.send(block).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Opus decode error: {e}");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "ALSA audio bridge lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
