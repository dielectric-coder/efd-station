use std::sync::Arc;
use std::time::Duration;

use efd_proto::{AudioChunk, AudioSource, Capabilities, CatCommand, FftBins, RadioState, TxAudio};
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

    /// What the active source supports; sent to every client on connect.
    pub capabilities: Capabilities,

    /// Latest DRM decoder status. `None` when no DRM bridge is running
    /// (mode isn't DRM) or the bridge hasn't produced a frame yet.
    pub drm_status_rx: watch::Receiver<Option<efd_proto::DrmStatus>>,

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

        // --- Source backend + capabilities ---
        // Only FDM-DUO is wired up today; wiring other backends is a matter
        // of implementing their capture task in efd-iq and choosing the
        // variant here from config.
        let source_cfg = efd_iq::SourceConfig::FdmDuo(efd_iq::FdmDuoConfig {
            vendor_id: config.usb.vendor_id,
            product_id: config.usb.product_id,
        });
        let source_caps = source_cfg.capabilities();

        // --- IQ capture task (skipped when the source has no IQ) ---
        let (iq_center_tx, iq_center_rx) = tokio::sync::watch::channel(0u64);
        if source_caps.has_iq {
            let tx = iq_tx.clone();
            let c = cancel.clone();
            let handle = efd_iq::spawn_source(source_cfg.clone(), tx, iq_center_tx, c);
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
        // Wideband-SSB audio-IF stream the demod emits under Mode::DRM; the
        // DRM bridge will subscribe here once it's wired up. Kept live at
        // pipeline scope so the demod's sender never sees a fully-dropped
        // channel even while no DRM bridge is running.
        let (drm_if_tx, _drm_if_rx) =
            broadcast::channel::<efd_dsp::AudioBlock>(16);
        {
            let iq_rx = iq_tx.subscribe();
            let demod_cfg = efd_dsp::DemodConfig {
                input_rate: config.dsp.sample_rate,
                output_rate: config.audio.sample_rate,
                mode: efd_proto::Mode::USB,
            };
            let dtx = demod_audio_tx.clone();
            let drm_tx = Some(drm_if_tx.clone());
            let c = cancel.clone();
            let handle = efd_dsp::spawn_demod_task(
                iq_rx,
                dtx,
                drm_tx,
                demod_cfg,
                demod_tuning_rx,
                c,
            );
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("demod task exited cleanly"),
                    Ok(Err(e)) => error!("demod task error: {e}"),
                    Err(e) => error!("demod task panicked: {e}"),
                }
            });
            tasks.push(("demod", handle));
        }

        // --- DRM supervisor ---
        // Brings the DREAM subprocess bridge up/down as the client switches
        // in and out of Mode::DRM. Only runs when the source can supply IQ.
        // A pipeline-scope `drm_status` watch is always present; the
        // supervisor forwards the active bridge's status into it when DRM
        // is running, and resets it to None when the bridge stops.
        let (drm_status_tx, drm_status_rx) =
            watch::channel::<Option<efd_proto::DrmStatus>>(None);
        if source_caps.has_iq {
            let drm_if_tx_for_drm = drm_if_tx.clone();
            let audio_tx_for_drm = demod_audio_tx.clone();
            let mode_rx = demod_mode_tx.subscribe();
            let status_tx = drm_status_tx.clone();
            let c = cancel.clone();
            let drm_cfg = efd_dsp::DrmConfig {
                dream_binary: config.drm.dream_binary.clone(),
                input_sink: config.drm.input_sink.clone(),
                output_sink: config.drm.output_sink.clone(),
                dream_rate: config.audio.sample_rate,
                flip_spectrum: config.drm.flip_spectrum,
            };
            let handle = tokio::spawn(async move {
                run_drm_supervisor(
                    drm_cfg,
                    drm_if_tx_for_drm,
                    audio_tx_for_drm,
                    mode_rx,
                    status_tx,
                    c,
                )
                .await;
            });
            tasks.push(("drm_supervisor", handle));
        }

        // --- USB RX audio capture (radio's hardware demod output) ---
        //
        // Gate on a real snd_pcm_open probe, not just sysfs resolution.
        // On the FDM-DUO another process (PipeWire/PulseAudio) can grab
        // the USB audio card, so the device name exists but opening it
        // fails with ENOTSUPP (errno 524) — we need to learn that now,
        // so `has_usb_audio` is accurate before we advertise capabilities.
        //
        // `EFD_AUDIO_FILE_RX=/path/to/file.wav` substitutes a file
        // source for USB RX (Phase 2 of the pipeline refactor — see
        // docs/CM5-sdr-backend-pipeline.drawio). Useful for testing the
        // whole audio pipeline without a radio. File must be 48 kHz;
        // no resampling yet.
        let (usb_rx_tx, usb_rx_rx) = mpsc::channel::<efd_audio::PcmBlock>(64);
        let usb_audio_live = if let Ok(path) = std::env::var("EFD_AUDIO_FILE_RX") {
            let path = std::path::PathBuf::from(&path);
            info!(file = %path.display(), "audio source: file (EFD_AUDIO_FILE_RX)");
            let file_cfg = efd_audio::FileSourceConfig {
                path,
                sample_rate: config.audio.sample_rate,
            };
            let c = cancel.clone();
            let handle = efd_audio::spawn_file_source_task(file_cfg, usb_rx_tx, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("file source exited cleanly"),
                    Ok(Err(e)) => error!("file source error: {e}"),
                    Err(e) => error!("file source panicked: {e}"),
                }
            });
            tasks.push(("file_source", handle));
            true
        } else {
            match efd_audio::resolve_device(&config.audio.rx_device, true) {
                Some(rx_dev) if efd_audio::probe_capture(&rx_dev) => {
                    info!(device = %rx_dev, "USB RX audio capture device");
                    let usb_rx_cfg = efd_audio::UsbRxConfig {
                        device: rx_dev,
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
                    true
                }
                Some(rx_dev) => {
                    drop(usb_rx_tx);
                    info!(device = %rx_dev, "USB RX audio unavailable (probe failed)");
                    false
                }
                None => {
                    drop(usb_rx_tx);
                    info!("USB RX audio disabled (device not found)");
                    false
                }
            }
        };

        // --- Audio source mux → Opus encoder → broadcast<AudioChunk> ---
        let (audio_source_tx, audio_source_rx) = watch::channel(AudioSource::RadioUsb);
        {
            let atx = audio_tx.clone();
            let c = cancel.clone();
            let mode_rx = demod_mode_tx.subscribe();
            let sample_rate = config.audio.sample_rate;
            let handle = tokio::spawn(async move {
                encode_audio_mux(
                    demod_audio_rx,
                    usb_rx_rx,
                    audio_source_rx,
                    mode_rx,
                    sample_rate,
                    atx,
                    c,
                )
                .await;
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
        if let Some(tx_dev) = efd_audio::resolve_device(&config.audio.tx_device, false) {
            info!(device = %tx_dev, "USB TX audio playback device");
            let usb_tx_cfg = efd_audio::UsbTxConfig {
                device: tx_dev,
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
        } else {
            info!("USB TX audio disabled (device not found)");
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

        // --- RadioState bridge: broadcast → watch (latest-value cache) ---
        // Consumers like the rigctld-compatible responder want the latest
        // known RadioState without having to drain a broadcast channel.
        let (state_watch_tx, state_watch_rx) =
            tokio::sync::watch::channel::<Option<RadioState>>(None);
        {
            let mut rx = state_tx.subscribe();
            let c = cancel.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        r = rx.recv() => match r {
                            Ok(s) => { let _ = state_watch_tx.send(Some(s)); }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            });
            tasks.push(("state_watch_bridge", handle));
        }

        // --- rigctld-compatible TCP responders (WSJT-X / FLDIGI / etc.) ---
        // Port A fronts the FDM-DUO; bound whenever hardware CAT is present.
        // Port B fronts the software demod; bound whenever IQ is available.
        if source_caps.has_hardware_cat {
            match config.cat.responder_fdmduo_bind.parse() {
                Ok(bind_addr) => {
                    let cfg = efd_cat::ResponderConfig {
                        bind_addr,
                        label: "fdmduo-front",
                    };
                    let backend = efd_cat::Backend::Hardware {
                        cat_tx: cat_tx.clone(),
                    };
                    let c = cancel.clone();
                    let handle = efd_cat::spawn_responder(cfg, backend, state_watch_rx.clone(), c);
                    let handle = tokio::spawn(async move {
                        match handle.await {
                            Ok(Ok(())) => info!("rigctld fdmduo-front exited cleanly"),
                            Ok(Err(e)) => error!("rigctld fdmduo-front error: {e}"),
                            Err(e) => error!("rigctld fdmduo-front panicked: {e}"),
                        }
                    });
                    tasks.push(("rigctld_hw", handle));
                }
                Err(e) => {
                    error!(
                        bind = %config.cat.responder_fdmduo_bind,
                        "invalid responder_fdmduo_bind, skipping FDM-DUO front: {e}"
                    );
                }
            }
        }
        if source_caps.has_iq {
            match config.cat.responder_demod_bind.parse() {
                Ok(bind_addr) => {
                    let cfg = efd_cat::ResponderConfig {
                        bind_addr,
                        label: "demod-front",
                    };
                    let backend = efd_cat::Backend::Demod {
                        cat_tx: cat_tx.clone(),
                        demod_mode: demod_mode_tx.clone(),
                    };
                    let c = cancel.clone();
                    let handle = efd_cat::spawn_responder(cfg, backend, state_watch_rx.clone(), c);
                    let handle = tokio::spawn(async move {
                        match handle.await {
                            Ok(Ok(())) => info!("rigctld demod-front exited cleanly"),
                            Ok(Err(e)) => error!("rigctld demod-front error: {e}"),
                            Err(e) => error!("rigctld demod-front panicked: {e}"),
                        }
                    });
                    tasks.push(("rigctld_demod", handle));
                }
                Err(e) => {
                    error!(
                        bind = %config.cat.responder_demod_bind,
                        "invalid responder_demod_bind, skipping demod front: {e}"
                    );
                }
            }
        }

        info!(tasks = tasks.len(), "pipeline started");

        // has_usb_audio reflects *runtime* availability (ALSA device
        // resolved at startup), not just driver intent — the FDM-DUO
        // source says it supports USB audio, but if the cable isn't
        // plugged in the device won't resolve and the client must know.
        let capabilities = Capabilities {
            source: source_caps.kind,
            has_iq: source_caps.has_iq,
            has_tx: source_caps.has_tx,
            has_hardware_cat: source_caps.has_hardware_cat,
            has_usb_audio: source_caps.has_usb_audio && usb_audio_live,
            supported_demod_modes: source_caps.supported_demod_modes,
        };

        Self {
            fft_tx,
            state_tx,
            audio_tx,
            cat_tx,
            tx_audio_tx,
            demod_mode_tx,
            audio_source_tx,
            capabilities,
            drm_status_rx,
            cancel,
            tasks,
        }
    }

    /// Minimal pipeline for the DRM file-playback test.
    ///
    /// Skips IQ capture / FFT / demod / CAT / rigctld / USB audio entirely.
    /// A file-source task reads a mono audio-IF WAV or FLAC and publishes
    /// `AudioBlock`s to the same `drm_if_tx` broadcast the demod normally
    /// writes under Mode::DRM, so the real `efd-dsp::drm` bridge +
    /// encode_audio_mux + WS downstream chain runs unchanged.
    ///
    /// A small synth task emits a fixed RadioState (mode=DRM, 10 kHz BW)
    /// on `state_tx` every 500 ms so the client UI gates its DRM display
    /// rows on (otherwise the screen looks blank).
    ///
    /// When the file source reaches EOF it fires the pipeline cancel
    /// token; main.rs's shutdown path then winds down axum + pipeline.
    pub fn start_drm_file_test(
        config: &Config,
        file_path: std::path::PathBuf,
        cancel: CancellationToken,
    ) -> Self {
        // Same channel set as the real pipeline — WS handlers don't know
        // (or need to know) which variant they're wired to.
        let (fft_tx, _) = broadcast::channel::<Arc<FftBins>>(8);
        let (state_tx, _) = broadcast::channel::<RadioState>(16);
        let (audio_tx, _) = broadcast::channel::<AudioChunk>(32);
        let (cat_tx, mut cat_rx) = mpsc::channel::<CatCommand>(64);
        let (tx_audio_tx, mut tx_audio_rx) = mpsc::channel::<TxAudio>(64);
        let (demod_audio_tx, demod_audio_rx) = mpsc::channel::<efd_dsp::AudioBlock>(64);
        let (usb_rx_tx, usb_rx_rx) = mpsc::channel::<efd_audio::PcmBlock>(64);
        drop(usb_rx_tx); // no USB audio producer in file-test mode
        let (demod_mode_tx, _demod_mode_rx_unused) =
            watch::channel::<Option<efd_proto::Mode>>(Some(efd_proto::Mode::DRM));
        let (audio_source_tx, audio_source_rx) =
            watch::channel(AudioSource::SoftwareDemod);

        let mut tasks: Vec<(&'static str, JoinHandle<()>)> = Vec::new();

        // Drain the unused upstream channels so senders held by WS handlers
        // don't stall when clients send CAT / TX audio we ignore here.
        {
            let c = cancel.clone();
            tasks.push((
                "drain_upstream",
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            biased;
                            _ = c.cancelled() => break,
                            _ = cat_rx.recv() => {}
                            _ = tx_audio_rx.recv() => {}
                        }
                    }
                }),
            ));
        }

        // --- DRM bridge in File mode ---
        //
        // DREAM reads the audio-IF file directly via `-f`; no Rust-side
        // file reader, no `drm_if` broadcast, no `drm_in` null sink, no
        // pacat — all eliminated relative to the AudioBroadcast path.
        // When DREAM hits EOF the bridge exits cleanly and we propagate
        // cancel so axum shuts down.
        let drm_cfg = efd_dsp::DrmConfig {
            dream_binary: config.drm.dream_binary.clone(),
            input_sink: config.drm.input_sink.clone(),
            output_sink: config.drm.output_sink.clone(),
            dream_rate: config.audio.sample_rate,
            flip_spectrum: config.drm.flip_spectrum,
        };
        let bridge = efd_dsp::spawn_drm_bridge(
            drm_cfg,
            efd_dsp::DrmInput::File(file_path.clone()),
            demod_audio_tx.clone(),
            cancel.clone(),
        );
        let drm_status_rx = bridge.status_rx;
        let cancel_done = cancel.clone();
        let bridge_handle = tokio::spawn(async move {
            match bridge.join.await {
                Ok(Ok(())) => info!("DRM bridge (file mode) exited cleanly"),
                Ok(Err(e)) => error!("DRM bridge error: {e}"),
                Err(e) => error!("DRM bridge panicked: {e}"),
            }
            cancel_done.cancel();
        });
        tasks.push(("drm_bridge", bridge_handle));

        // --- Opus encoder mux (unchanged from real pipeline) ---
        {
            let atx = audio_tx.clone();
            let c = cancel.clone();
            let mode_rx = demod_mode_tx.subscribe();
            let handle = tokio::spawn(async move {
                encode_audio_mux(
                    demod_audio_rx,
                    usb_rx_rx,
                    audio_source_rx,
                    mode_rx,
                    48_000,
                    atx,
                    c,
                )
                .await;
            });
            tasks.push(("opus_encoder", handle));
        }

        // --- Synthetic RadioState so the client UI gates correctly ---
        {
            let stx = state_tx.clone();
            let c = cancel.clone();
            let handle = tokio::spawn(async move {
                let state = RadioState {
                    vfo: efd_proto::Vfo::A,
                    freq_hz: 0,
                    mode: efd_proto::Mode::DRM,
                    filter_bw: "10.0k".into(),
                    att: false,
                    lp: false,
                    agc: efd_proto::AgcMode::Off,
                    agc_threshold: 0,
                    nr: false,
                    nb: false,
                    s_meter_db: -120.0,
                    tx: false,
                };
                let mut tick = tokio::time::interval(Duration::from_millis(500));
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        _ = tick.tick() => {
                            let _ = stx.send(state.clone());
                        }
                    }
                }
            });
            tasks.push(("state_synth", handle));
        }

        // ALSA playback intentionally omitted — under SSH sessions on the
        // CM5 there's no live audio context, and the point of the test is
        // to verify the WS audio path, not local speaker output.

        let capabilities = Capabilities {
            source: efd_proto::SourceKind::FdmDuo,
            has_iq: false,
            has_tx: false,
            has_hardware_cat: false,
            has_usb_audio: false,
            supported_demod_modes: vec![efd_proto::Mode::DRM],
        };

        info!(
            file = %file_path.display(),
            tasks = tasks.len(),
            "DRM file-test pipeline started"
        );

        Self {
            fft_tx,
            state_tx,
            audio_tx,
            cat_tx,
            tx_audio_tx,
            demod_mode_tx,
            audio_source_tx,
            capabilities,
            drm_status_rx,
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
    mut demod_mode_rx: watch::Receiver<Option<efd_proto::Mode>>,
    sample_rate: u32,
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
    // Audio-domain DSP block — sits on every path to Audio Out per the
    // pipeline drawio. Pass-through stub in Phase 1; real filters land
    // in a later phase. Flags are always-off by default so this is a
    // no-op today.
    let dsp = efd_dsp::AudioDsp::new();
    // Audio-rate IF bandpass — the "Audio source → IF demod" edge in
    // the drawio. Active only on the USB-RX / file path; the demod
    // task already filters the IQ → audio chain. Default bypass
    // until a narrow mode is selected.
    let mut audio_if = efd_dsp::AudioIfFilter::new(sample_rate as f32);
    let mut current_mode: Option<efd_proto::Mode> = *demod_mode_rx.borrow();
    audio_if.set_mode(current_mode);
    let mut dsp_scratch: Vec<f32> = Vec::with_capacity(efd_audio::OPUS_FRAME_SIZE);
    let mut demod_alive = true;
    let mut usb_alive = true;
    let mut logged_fallback = false;

    loop {
        // Resolve effective source: fall back to the other if the selected one
        // is unavailable (channel closed / not configured).
        let requested = *source_rx.borrow();
        let source = match requested {
            AudioSource::RadioUsb if !usb_alive => {
                if !logged_fallback {
                    tracing::warn!("USB RX unavailable, falling back to software demod");
                    logged_fallback = true;
                }
                AudioSource::SoftwareDemod
            }
            AudioSource::SoftwareDemod if !demod_alive => {
                if !logged_fallback {
                    tracing::warn!("software demod unavailable, falling back to USB RX");
                    logged_fallback = true;
                }
                AudioSource::RadioUsb
            }
            other => other,
        };
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = source_rx.changed() => {
                let new_src = *source_rx.borrow();
                info!(?new_src, "audio source changed");
                frame_buf.clear();
                logged_fallback = false;
                continue;
            }
            _ = demod_mode_rx.changed() => {
                let new_mode = *demod_mode_rx.borrow();
                if new_mode != current_mode {
                    current_mode = new_mode;
                    audio_if.set_mode(current_mode);
                }
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
                dsp_scratch.clear();
                dsp_scratch.extend_from_slice(&block.samples);
                dsp.process(&mut dsp_scratch);
                encode_samples(&dsp_scratch, &mut frame_buf, &mut encoder, &mut seq, &tx);
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
                dsp_scratch.clear();
                dsp_scratch.extend_from_slice(&block.samples);
                // Audio → IF demod → DSP → Audio Out.
                // IF filter is applied only to already-audio sources;
                // IQ → audio demod has its own filters upstream.
                audio_if.process(&mut dsp_scratch);
                dsp.process(&mut dsp_scratch);
                encode_samples(&dsp_scratch, &mut frame_buf, &mut encoder, &mut seq, &tx);
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

/// Watches `mode_rx` and brings the DRM bridge subprocess up/down.
///
/// When the client selects Mode::DRM, spawn the bridge (which loads its
/// own PipeWire null sinks and launches dream). When the client changes
/// away, cancel the bridge and await it. One bridge instance at a time.
///
/// While a bridge is active, a forwarder task copies its per-bridge
/// status watch into the pipeline-scope `status_tx` so WS handlers
/// don't have to re-subscribe on every mode change.
async fn run_drm_supervisor(
    cfg: efd_dsp::DrmConfig,
    drm_if_tx: broadcast::Sender<efd_dsp::AudioBlock>,
    audio_tx: mpsc::Sender<efd_dsp::AudioBlock>,
    mut mode_rx: watch::Receiver<Option<efd_proto::Mode>>,
    status_tx: watch::Sender<Option<efd_proto::DrmStatus>>,
    cancel: CancellationToken,
) {
    struct Active {
        cancel: CancellationToken,
        join: JoinHandle<Result<(), efd_dsp::DspError>>,
        forwarder: JoinHandle<()>,
    }
    let mut active: Option<Active> = None;

    loop {
        let want_drm = matches!(*mode_rx.borrow(), Some(efd_proto::Mode::DRM));

        match (&active, want_drm) {
            (None, true) => {
                info!("DRM supervisor: starting bridge");
                let bc = CancellationToken::new();
                let handles = efd_dsp::spawn_drm_bridge(
                    cfg.clone(),
                    efd_dsp::DrmInput::AudioBroadcast(drm_if_tx.subscribe()),
                    audio_tx.clone(),
                    bc.clone(),
                );
                let mut brs = handles.status_rx;
                let pipe_status = status_tx.clone();
                let fwd_cancel = bc.clone();
                let forwarder = tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = fwd_cancel.cancelled() => break,
                            r = brs.changed() => {
                                if r.is_err() { break; }
                                let _ = pipe_status.send(brs.borrow().clone());
                            }
                        }
                    }
                });
                active = Some(Active { cancel: bc, join: handles.join, forwarder });
            }
            (Some(_), false) => {
                info!("DRM supervisor: stopping bridge");
                let a = active.take().expect("active checked");
                a.cancel.cancel();
                let _ = a.forwarder.await;
                match a.join.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("DRM bridge exited with error: {e}"),
                    Err(e) => tracing::warn!("DRM bridge panicked: {e}"),
                }
                let _ = status_tx.send(None);
            }
            _ => {}
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(a) = active.take() {
                    a.cancel.cancel();
                    let _ = a.forwarder.await;
                    let _ = a.join.await;
                    let _ = status_tx.send(None);
                }
                return;
            }
            r = mode_rx.changed() => {
                if r.is_err() {
                    if let Some(a) = active.take() {
                        a.cancel.cancel();
                        let _ = a.forwarder.await;
                        let _ = a.join.await;
                        let _ = status_tx.send(None);
                    }
                    return;
                }
            }
        }
    }
}
