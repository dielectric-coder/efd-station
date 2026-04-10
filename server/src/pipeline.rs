use std::sync::Arc;
use std::time::Duration;

use efd_proto::{AudioChunk, CatCommand, FftBins, RadioState, TxAudio};
use tokio::sync::{broadcast, mpsc};
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

    pub(crate) cancel: CancellationToken,
    #[allow(dead_code)]
    tasks: Vec<(&'static str, JoinHandle<()>)>,
}

impl Pipeline {
    /// Create all channels, spawn all tasks.
    pub fn start(config: &Config) -> Self {
        let cancel = CancellationToken::new();

        // -- broadcast channels (fan-out) --
        let (iq_tx, _) = broadcast::channel::<Arc<efd_iq::IqBlock>>(16);
        let (fft_tx, _) = broadcast::channel::<Arc<FftBins>>(8);
        let (state_tx, _) = broadcast::channel::<RadioState>(16);
        let (audio_tx, _) = broadcast::channel::<AudioChunk>(32);

        // -- mpsc channels (single consumer) --
        let (cat_tx, cat_rx) = mpsc::channel::<CatCommand>(64);
        let (tx_audio_tx, tx_audio_rx) = mpsc::channel::<TxAudio>(64);
        // Internal mpsc for demod → Opus encoder → broadcast<AudioChunk>
        let (demod_audio_tx, demod_audio_rx) =
            mpsc::channel::<efd_dsp::AudioBlock>(64);

        let mut tasks: Vec<(&'static str, JoinHandle<()>)> = Vec::new();

        // --- IQ capture task ---
        {
            let iq_cfg = efd_iq::IqConfig {
                vendor_id: config.usb.vendor_id,
                product_id: config.usb.product_id,
            };
            let tx = iq_tx.clone();
            let c = cancel.clone();
            let handle = efd_iq::spawn_iq_capture(iq_cfg, tx, c);
            // Wrap the typed JoinHandle into a unit one for uniform storage
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("IQ capture exited cleanly"),
                    Ok(Err(e)) => error!("IQ capture error: {e}"),
                    Err(e) => error!("IQ capture panicked: {e}"),
                }
            });
            tasks.push(("iq_capture", handle));
        }

        // --- IQ forwarder: efd_iq::IqBlock → efd_dsp::IqBlock ---
        // Hoisted out so both FFT and demod can subscribe to dsp_iq_tx.
        let (dsp_iq_tx, _) = broadcast::channel::<Arc<efd_dsp::IqBlock>>(16);
        {
            let iq_rx = iq_tx.subscribe();
            let dsp_iq_tx2 = dsp_iq_tx.clone();
            let c = cancel.clone();
            let forward = tokio::spawn(async move {
                forward_iq_blocks(iq_rx, dsp_iq_tx2, c).await;
            });
            tasks.push(("iq_forward", forward));
        }

        // --- FFT task ---
        {
            let fft_cfg = efd_dsp::FftConfig {
                fft_size: config.dsp.fft_size,
                averaging: config.dsp.fft_averaging,
                center_freq_hz: 7_100_000, // updated by CAT state later
                span_hz: config.dsp.sample_rate,
                ref_level_db: -20.0,
            };
            let dsp_iq_rx = dsp_iq_tx.subscribe();
            let fft_tx2 = fft_tx.clone();
            let c = cancel.clone();
            let handle = efd_dsp::spawn_fft_task(dsp_iq_rx, fft_tx2, fft_cfg, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("FFT task exited cleanly"),
                    Ok(Err(e)) => error!("FFT task error: {e}"),
                    Err(e) => error!("FFT task panicked: {e}"),
                }
            });
            tasks.push(("fft", handle));
        }

        // --- Demod task ---
        {
            let dsp_iq_rx2 = dsp_iq_tx.subscribe();
            let demod_cfg = efd_dsp::DemodConfig {
                input_rate: config.dsp.sample_rate,
                output_rate: config.audio.sample_rate,
                mode: efd_proto::Mode::USB, // initial mode, updated by CAT state later
            };
            let dtx = demod_audio_tx;
            let c = cancel.clone();
            let handle = efd_dsp::spawn_demod_task(dsp_iq_rx2, dtx, demod_cfg, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("demod task exited cleanly"),
                    Ok(Err(e)) => error!("demod task error: {e}"),
                    Err(e) => error!("demod task panicked: {e}"),
                }
            });
            tasks.push(("demod", handle));
        }

        // --- Opus encoder bridge: demod AudioBlock → Opus → broadcast<AudioChunk> ---
        {
            let atx = audio_tx.clone();
            let c = cancel.clone();
            let handle = tokio::spawn(async move {
                encode_audio_to_opus(demod_audio_rx, atx, c).await;
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
            // ALSA gets raw PCM, not Opus. We need a separate mpsc for it.
            let (alsa_tx, alsa_rx) = mpsc::channel::<efd_audio::PcmBlock>(64);
            let atx = alsa_tx;
            let audio_rx_for_alsa = audio_tx.subscribe();
            let c = cancel.clone();
            // Bridge: decode Opus AudioChunk back to PCM for local playback
            let alsa_bridge = tokio::spawn(async move {
                decode_audio_for_alsa(audio_rx_for_alsa, atx, c).await;
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
                device: config.audio.alsa_device.clone(), // TODO: separate TX device config
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

        // --- CAT tasks ---
        {
            let cat_cfg = efd_cat::CatConfig {
                host: config.cat.rigctld_host.clone(),
                port: config.cat.rigctld_port,
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

        info!(tasks = tasks.len(), "pipeline started");

        Self {
            fft_tx,
            state_tx,
            audio_tx,
            cat_tx,
            tx_audio_tx,
            cancel,
            tasks,
        }
    }

    /// Graceful shutdown — cancel all tasks and wait for them to finish.
    #[allow(dead_code)]
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

/// Encode demod AudioBlocks to Opus AudioChunks for WS broadcast.
async fn encode_audio_to_opus(
    mut rx: mpsc::Receiver<efd_dsp::AudioBlock>,
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

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            block = rx.recv() => {
                let Some(block) = block else { break };

                for &sample in &block.samples {
                    frame_buf.push(sample);
                    if frame_buf.len() == efd_audio::OPUS_FRAME_SIZE {
                        match encoder.encode_float(&frame_buf) {
                            Ok(opus_data) => {
                                let chunk = AudioChunk { opus_data, seq };
                                seq = seq.wrapping_add(1);
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

/// Bridge efd_iq::IqBlock → efd_dsp::IqBlock (separate types to avoid crate coupling).
async fn forward_iq_blocks(
    mut rx: broadcast::Receiver<Arc<efd_iq::IqBlock>>,
    tx: broadcast::Sender<Arc<efd_dsp::IqBlock>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = async { rx.recv().await } => {
                match result {
                    Ok(block) => {
                        let dsp_block = Arc::new(efd_dsp::IqBlock {
                            samples: block.samples.clone(),
                            timestamp_us: block.timestamp_us,
                        });
                        let _ = tx.send(dsp_block);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "IQ forwarder lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
