use std::sync::Arc;
use std::time::Duration;

use efd_proto::{
    AudioChunk, Capabilities, CatCommand, ControlTarget, DeviceList, FftBins, RadioState,
    RecordingStatus, SourceClass, SourceKind, StateSnapshot, TxAudio,
};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::recording::{self, RecorderHandle};

/// Expand a leading `~` in a config-supplied path using the service
/// user's `HOME`. `PathBuf::from` doesn't do this automatically;
/// systemd gives us `HOME=/home/efd` when running as `User=efd`.
/// Falls back to `/tmp/efd-recordings` if `HOME` isn't set.
fn expand_home(raw: &str) -> std::path::PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = std::path::PathBuf::from(home);
            p.push(rest);
            return p;
        }
        return std::path::PathBuf::from("/tmp/efd-recordings");
    }
    std::path::PathBuf::from(raw)
}

/// Internal audio-routing selector driving `encode_audio_mux`. Maps
/// 1:1 onto the client-facing `SourceClass` today (Audio → RadioUsb,
/// Iq → SoftwareDemod) but kept distinct so the pipeline can
/// evolve — phase 2's runtime device model will let this be driven
/// by `SelectDevice` too, not just `SelectSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRouting {
    /// Audio comes from the FDM-DUO USB audio passthrough (or, in
    /// portable-radio configs, a USB audio dongle). Corresponds to
    /// `SourceClass::Audio`.
    RadioUsb,
    /// Audio comes from the software demod chain, driven by an IQ
    /// source. Corresponds to `SourceClass::Iq`.
    SoftwareDemod,
}

impl From<SourceClass> for AudioRouting {
    fn from(src: SourceClass) -> Self {
        match src {
            SourceClass::Audio => AudioRouting::RadioUsb,
            SourceClass::Iq => AudioRouting::SoftwareDemod,
        }
    }
}

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
    pub audio_source_tx: watch::Sender<AudioRouting>,

    /// Runtime DREAM spectrum-flip flag (`-p`). Updated by the client;
    /// the DRM supervisor restarts the bridge when this changes.
    pub flip_spectrum_tx: watch::Sender<bool>,

    /// What the active source supports; sent to every client on connect.
    pub capabilities: Capabilities,

    /// Latest DRM decoder status. `None` when no DRM bridge is running
    /// (mode isn't DRM) or the bridge hasn't produced a frame yet.
    pub drm_status_rx: watch::Receiver<Option<efd_proto::DrmStatus>>,

    /// Current view of discovered devices (phase 2). Updated by the
    /// upstream `EnumerateDevices` handler when it re-runs discovery.
    /// Downstream subscribes and pushes `ServerMsg::DeviceList` on
    /// change.
    pub device_list_tx: watch::Sender<DeviceList>,

    /// Current session snapshot (phase 2). Mirrors the tuning state
    /// that will be persisted on shutdown. Updated by the internal
    /// "snapshot tracker" task as `RadioState` ticks in, and by the
    /// upstream handler when the client changes selections / DSP
    /// toggles / active decoders.
    pub snapshot_tx: watch::Sender<StateSnapshot>,

    /// Phase 3e: process-respawn hot-swap trigger. Upstream writes
    /// `true` when a client requests a cross-device swap that the
    /// in-process pipeline can't accommodate today; `main.rs` checks
    /// after the HTTP server returns, saves the snapshot, and exits
    /// cleanly so systemd's `Restart=always` brings the service
    /// back with the newly-selected device active. True in-process
    /// hot-swap is phase 3f.
    pub restart_requested_tx: watch::Sender<bool>,

    /// Phase 4 REC: command sink for the rec-controller. WS upstream
    /// clones this and sends `StartRecording` / `StopRecording`.
    pub recorder: RecorderHandle,
    /// Latest `RecordingStatus` published by the active recorder
    /// (or the "inactive" default when nothing is recording).
    /// Downstream subscribes and pushes on change.
    pub rec_status_tx: watch::Sender<RecordingStatus>,

    pub(crate) cancel: CancellationToken,
    tasks: Vec<(&'static str, JoinHandle<()>)>,
}

impl Pipeline {
    /// Create all channels, spawn all tasks.
    ///
    /// `devices` is the set of devices discovery turned up at startup;
    /// it's exposed to clients via `EnumerateDevices` / the downstream
    /// push. `initial_snapshot` is the persisted session state
    /// (validated against `devices` by the caller) — the pipeline uses
    /// it to seed the live snapshot but does not yet honour its
    /// `active_device` for pipeline routing; that's phase 3's
    /// hot-swap work.
    pub fn start(
        config: &Config,
        devices: DeviceList,
        initial_snapshot: StateSnapshot,
    ) -> Self {
        let cancel = CancellationToken::new();

        // -- broadcast channels (fan-out) --
        // iq_tx carries raw IQ from the capture driver; iq_clean_tx is
        // the post-noise-blanker stream the demod consumes. FFT stays
        // on the raw feed so the waterfall shows the actual spectrum.
        // pcm_tx carries post-DSP audio samples from encode_audio_mux,
        // so the REC recorder can write raw PCM without an Opus
        // decoder round-trip.
        let (iq_tx, _) = broadcast::channel::<Arc<efd_iq::IqBlock>>(16);
        let (iq_clean_tx, _) = broadcast::channel::<Arc<efd_iq::IqBlock>>(16);
        let (fft_tx, _) = broadcast::channel::<Arc<FftBins>>(8);
        let (state_tx, _) = broadcast::channel::<RadioState>(16);
        let (audio_tx, _) = broadcast::channel::<AudioChunk>(32);
        let (pcm_tx, _) = broadcast::channel::<Arc<Vec<f32>>>(32);

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

        // --- Pre-IF noise blanker (NB box in the pipeline drawio) ---
        // Runs on every IQ block regardless of `nb_on`; the flag just
        // selects pass-through-vs-blank inside the task. Seeded from
        // the loaded snapshot so the persisted toggle takes effect
        // before any client connects.
        let (nb_enabled_tx, nb_enabled_rx) =
            tokio::sync::watch::channel::<bool>(initial_snapshot.nb_on);
        if source_caps.has_iq {
            let iq_in = iq_tx.subscribe();
            let iq_out = iq_clean_tx.clone();
            let c = cancel.clone();
            let handle = efd_dsp::spawn_noise_blanker(iq_in, iq_out, nb_enabled_rx, c);
            let handle = tokio::spawn(async move {
                match handle.await {
                    Ok(Ok(())) => info!("NB task exited cleanly"),
                    Ok(Err(e)) => error!("NB task error: {e}"),
                    Err(e) => error!("NB task panicked: {e}"),
                }
            });
            tasks.push(("nb", handle));
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
            // Demod consumes the *post-NB* stream, per the pipeline
            // drawio: IQ source → NB → IQ→IF → IF demod.
            let iq_rx = iq_clean_tx.subscribe();
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
        // Runtime flip-spectrum watch. Seeded from config at startup so
        // existing configs keep their behaviour; the client can flip it
        // at any time via `ClientMsg::SetDrmFlipSpectrum`.
        let (flip_spectrum_tx, _flip_spectrum_rx) =
            watch::channel::<bool>(config.drm.flip_spectrum);
        if source_caps.has_iq {
            let drm_if_tx_for_drm = drm_if_tx.clone();
            let audio_tx_for_drm = demod_audio_tx.clone();
            let mode_rx = demod_mode_tx.subscribe();
            let flip_rx = flip_spectrum_tx.subscribe();
            let status_tx = drm_status_tx.clone();
            let c = cancel.clone();
            // Base DrmConfig; supervisor overrides `flip_spectrum` from
            // the watch on each bridge spawn so the value is always
            // fresh and runtime-toggleable.
            let drm_cfg = efd_dsp::DrmConfig {
                dream_binary: config.drm.dream_binary.clone(),
                input_sink: config.drm.input_sink.clone(),
                output_sink: config.drm.output_sink.clone(),
                dream_rate: config.audio.sample_rate,
                flip_spectrum: false,
            };
            let handle = tokio::spawn(async move {
                run_drm_supervisor(
                    drm_cfg,
                    drm_if_tx_for_drm,
                    audio_tx_for_drm,
                    mode_rx,
                    flip_rx,
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
            // Honour the persisted `active_device` over the config-level
            // rx_device when the loaded snapshot picked a specific audio
            // input (FDM-DUO USB audio or a generic capture card
            // surfaced by `/proc/asound/cards`). Falls back to the
            // configured value (typically "auto" → FDM-DUO discovery).
            let snapshot_rx = active_audio_device(&initial_snapshot);
            let rx_source = snapshot_rx
                .clone()
                .or_else(|| efd_audio::resolve_device(&config.audio.rx_device, true));
            if let Some(ref d) = snapshot_rx {
                info!(device = %d, "using saved active_device as audio RX");
            }
            match rx_source {
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

        // --- Audio-domain DSP flags (DNB / DNR / DNF / APF) ---
        // Seeded from the loaded snapshot; updated by the snapshot
        // DSP propagator below whenever the client toggles a stage.
        let (dsp_flags_tx, dsp_flags_rx) =
            watch::channel::<efd_dsp::AudioDspFlags>(efd_dsp::AudioDspFlags {
                dnb: initial_snapshot.dnb_on,
                dnr: initial_snapshot.dnr_on,
                dnf: initial_snapshot.dnf_on,
                apf: initial_snapshot.apf_on,
            });

        // --- Audio source mux → Opus encoder → broadcast<AudioChunk> ---
        // Initial routing reflects the user's last pick. `SourceKind`
        // alone can't tell us which path the FDM-DUO was picked from
        // (its kind is `FdmDuo` in both lists), so we defer to
        // `active_audio_device` — if that returns Some, the id
        // matched an ALSA `hw:N,D` / `plughw:N,D` audio input, and
        // we start in `RadioUsb`. Everything else (IQ kinds, empty
        // snapshot) falls through to the class-based default.
        let initial_routing = if active_audio_device(&initial_snapshot).is_some() {
            AudioRouting::RadioUsb
        } else {
            initial_snapshot
                .active_device
                .as_ref()
                .map(|d| AudioRouting::from(d.kind.class()))
                .unwrap_or(AudioRouting::RadioUsb)
        };
        let (audio_source_tx, audio_source_rx) = watch::channel(initial_routing);
        {
            let atx = audio_tx.clone();
            let ptx = pcm_tx.clone();
            let c = cancel.clone();
            let mode_rx = demod_mode_tx.subscribe();
            let sample_rate = config.audio.sample_rate;
            let flags_rx = dsp_flags_rx.clone();
            let handle = tokio::spawn(async move {
                encode_audio_mux(
                    demod_audio_rx,
                    usb_rx_rx,
                    audio_source_rx,
                    mode_rx,
                    flags_rx,
                    sample_rate,
                    atx,
                    ptx,
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
            match config.cat.responder_fdmduo_bind.parse::<std::net::SocketAddr>() {
                Ok(bind_addr) => {
                    if !bind_addr.ip().is_loopback() {
                        warn!(
                            bind = %bind_addr,
                            "rigctld fdmduo-front is on a non-loopback interface and has NO auth. \
                             Any host on this network can retune the radio. \
                             Prefer 127.0.0.1 and reach it from the client via: \
                             ssh -L 4532:localhost:4532 <pi>"
                        );
                    }
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
            match config.cat.responder_demod_bind.parse::<std::net::SocketAddr>() {
                Ok(bind_addr) => {
                    if !bind_addr.ip().is_loopback() {
                        warn!(
                            bind = %bind_addr,
                            "rigctld demod-front is on a non-loopback interface and has NO auth. \
                             Any host on this network can retune the demod. \
                             Prefer 127.0.0.1 and reach it from the client via: \
                             ssh -L 4533:localhost:4533 <pi>"
                        );
                    }
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

        // --- Phase 2 state plumbing ---
        // device_list_tx publishes the current enumeration; upstream
        // `EnumerateDevices` can re-run discovery into this sender.
        let (device_list_tx, _device_list_rx) = watch::channel(devices);

        // Phase 3e: process-respawn swap signal.
        let (restart_requested_tx, _restart_requested_rx) = watch::channel(false);

        // --- Phase 4 REC: rec-controller ---
        let (rec_status_tx, _rec_status_rx) = watch::channel::<RecordingStatus>(
            RecordingStatus {
                active: false,
                kind: None,
                path: None,
                bytes_written: 0,
                duration_s: None,
            },
        );
        let rec_dir = expand_home(&config.recording.directory);
        let (recorder, rec_ctrl_handle) = recording::spawn_controller(
            iq_tx.clone(),
            pcm_tx.clone(),
            rec_status_tx.clone(),
            rec_dir,
            recording::RecorderRates {
                iq_sample_rate: config.dsp.sample_rate,
                audio_sample_rate: config.audio.sample_rate,
            },
            cancel.clone(),
        );
        tasks.push(("rec_controller", rec_ctrl_handle));

        // snapshot_tx holds the live session snapshot. Seeded from the
        // persisted snapshot the caller loaded/validated, then kept
        // current by the snapshot-tracker task below.
        let (snapshot_tx, _snapshot_rx) = watch::channel(initial_snapshot);

        // Snapshot tracker — subscribes to `state_tx` so the persisted
        // snapshot's freq / mode / filter_bw_hz match whatever the
        // radio is actually doing when we save on shutdown.
        //
        // Uses `send_if_modified` so the watch only notifies when a
        // tuning field *actually* changed. Without this, the CAT
        // poll cadence (~5 Hz) would fan `ServerMsg::StateSnapshot`
        // to every connected client every tick, even when the radio
        // is sitting still.
        {
            let mut state_sub = state_tx.subscribe();
            let snap_tx = snapshot_tx.clone();
            let c = cancel.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        r = state_sub.recv() => match r {
                            Ok(st) => {
                                snap_tx.send_if_modified(|s| {
                                    let mut changed = false;
                                    if s.freq_hz != st.freq_hz { s.freq_hz = st.freq_hz; changed = true; }
                                    if s.mode != st.mode { s.mode = st.mode; changed = true; }
                                    if s.filter_bw_hz != st.filter_bw_hz { s.filter_bw_hz = st.filter_bw_hz; changed = true; }
                                    if s.rit_hz != st.rit_hz { s.rit_hz = st.rit_hz; changed = true; }
                                    if s.xit_hz != st.xit_hz { s.xit_hz = st.xit_hz; changed = true; }
                                    if s.if_offset_hz != st.if_offset_hz { s.if_offset_hz = st.if_offset_hz; changed = true; }
                                    changed
                                });
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        },
                    }
                }
            });
            tasks.push(("snapshot_tracker", handle));
        }

        // Snapshot → DSP-flag propagator — watches the session
        // snapshot and pushes any change to `nb_on` / `dnb_on` /
        // `dnr_on` / `dnf_on` / `apf_on` out to the live pipeline's
        // derived watches. One place to read the canonical toggle
        // state, so future writers (LoadState, startup seed, WS
        // SetNb/SetDnb/...) stay consistent without duplicating
        // derive-logic across handlers.
        {
            let mut snap_sub = snapshot_tx.subscribe();
            let nb_tx = nb_enabled_tx.clone();
            let flags_tx = dsp_flags_tx.clone();
            let c = cancel.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        r = snap_sub.changed() => {
                            if r.is_err() { break; }
                            let s = snap_sub.borrow_and_update();
                            nb_tx.send_if_modified(|cur| {
                                if *cur != s.nb_on { *cur = s.nb_on; true } else { false }
                            });
                            let new_flags = efd_dsp::AudioDspFlags {
                                dnb: s.dnb_on,
                                dnr: s.dnr_on,
                                dnf: s.dnf_on,
                                apf: s.apf_on,
                            };
                            flags_tx.send_if_modified(|cur| {
                                if *cur != new_flags { *cur = new_flags; true } else { false }
                            });
                        }
                    }
                }
            });
            tasks.push(("snapshot_dsp_propagator", handle));
        }

        info!(tasks = tasks.len(), "pipeline started");

        // has_usb_audio reflects *runtime* availability (ALSA device
        // resolved at startup), not just driver intent — the FDM-DUO
        // source says it supports USB audio, but if the cable isn't
        // plugged in the device won't resolve and the client must know.
        let control_target = control_target_for(initial_routing, source_caps.kind);
        info!(
            ?initial_routing,
            source = ?source_caps.kind,
            ?control_target,
            "control target resolved"
        );
        let capabilities = Capabilities {
            source: source_caps.kind,
            has_iq: source_caps.has_iq,
            has_tx: source_caps.has_tx,
            has_hardware_cat: source_caps.has_hardware_cat,
            has_usb_audio: source_caps.has_usb_audio && usb_audio_live,
            supported_demod_modes: source_caps.supported_demod_modes,
            // Phase-1 stub — none of the rework-era decoders are
            // wired up in the pipeline yet. Phase 3 populates this
            // from a real capability probe against efd-dsp.
            supported_decoders: Vec::new(),
            drm_flip_spectrum: config.drm.flip_spectrum,
            control_target,
        };

        Self {
            fft_tx,
            state_tx,
            audio_tx,
            cat_tx,
            tx_audio_tx,
            demod_mode_tx,
            audio_source_tx,
            flip_spectrum_tx,
            capabilities,
            drm_status_rx,
            device_list_tx,
            snapshot_tx,
            restart_requested_tx,
            recorder,
            rec_status_tx,
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
            watch::channel(AudioRouting::SoftwareDemod);

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
        // File-test mode doesn't expose DSP toggles to a client yet —
        // seed the flags watch with all-off and never mutate it.
        let (_file_test_dsp_flags_tx, file_test_dsp_flags_rx) =
            watch::channel::<efd_dsp::AudioDspFlags>(Default::default());
        // File-test PCM broadcast is there for API parity; no recorder
        // is wired up on this path, so the subscriber count is always 0
        // and `send` just returns Err (ignored).
        let (file_test_pcm_tx, _) = broadcast::channel::<Arc<Vec<f32>>>(4);
        {
            let atx = audio_tx.clone();
            let ptx = file_test_pcm_tx.clone();
            let c = cancel.clone();
            let mode_rx = demod_mode_tx.subscribe();
            let flags_rx = file_test_dsp_flags_rx.clone();
            let handle = tokio::spawn(async move {
                encode_audio_mux(
                    demod_audio_rx,
                    usb_rx_rx,
                    audio_source_rx,
                    mode_rx,
                    flags_rx,
                    48_000,
                    atx,
                    ptx,
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
                    filter_bw_hz: Some(10_000.0),
                    att: false,
                    lp: false,
                    agc: efd_proto::AgcMode::Off,
                    agc_threshold: 0,
                    nr: false,
                    nb: false,
                    s_meter_db: -120.0,
                    tx: false,
                    rit_hz: 0,
                    rit_on: false,
                    xit_hz: 0,
                    xit_on: false,
                    if_offset_hz: 0,
                    snr_db: None,
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
            supported_decoders: Vec::new(),
            drm_flip_spectrum: false,
            // File-test synthesizes a FLAC-driven DRM pipeline — no
            // live radio or SDR to control. Greyed controls are
            // correct; the client sees mode/BW read-only.
            control_target: ControlTarget::None,
        };

        // Flip-spectrum watch exists for API parity with the live
        // pipeline; file-test DRM doesn't use it — `dream -f` path
        // decides its own flip from the file contents.
        let (flip_spectrum_tx, _) = watch::channel::<bool>(false);

        // File-test path carries no real device list or persisted
        // state; seed the watches with empty / default values so the
        // struct shape matches the live pipeline.
        let (device_list_tx, _) = watch::channel(DeviceList {
            audio_devices: Vec::new(),
            iq_devices: Vec::new(),
            active: None,
        });
        let (snapshot_tx, _) = watch::channel(crate::persistence::default_snapshot());
        let (restart_requested_tx, _) = watch::channel(false);

        // File-test pipeline: no live IQ source, so REC has nothing
        // to record. Provide a stub handle so the struct shape matches
        // the live pipeline; the mpsc receiver is dropped and any
        // StartRecording command sent through it simply fails
        // silently (send returns Err immediately).
        let (stub_cmd_tx, _stub_cmd_rx) = mpsc::channel::<recording::RecCmd>(1);
        let recorder = recording::RecorderHandle { cmd_tx: stub_cmd_tx };
        let (rec_status_tx, _) = watch::channel::<RecordingStatus>(RecordingStatus {
            active: false,
            kind: None,
            path: None,
            bytes_written: 0,
            duration_s: None,
        });

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
            flip_spectrum_tx,
            capabilities,
            drm_status_rx,
            device_list_tx,
            snapshot_tx,
            restart_requested_tx,
            recorder,
            rec_status_tx,
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
#[allow(clippy::too_many_arguments)]
async fn encode_audio_mux(
    mut demod_rx: mpsc::Receiver<efd_dsp::AudioBlock>,
    mut usb_rx: mpsc::Receiver<efd_audio::PcmBlock>,
    mut source_rx: watch::Receiver<AudioRouting>,
    mut demod_mode_rx: watch::Receiver<Option<efd_proto::Mode>>,
    mut dsp_flags_rx: watch::Receiver<efd_dsp::AudioDspFlags>,
    sample_rate: u32,
    tx: broadcast::Sender<AudioChunk>,
    pcm_tx: broadcast::Sender<Arc<Vec<f32>>>,
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
    // pipeline drawio. Stage implementations are still pass-through
    // stubs (phase 3b); phase 3a just wires the flags from the
    // session snapshot so client-side toggles actually reach here.
    let mut dsp = efd_dsp::AudioDsp::new();
    dsp.set_flags(*dsp_flags_rx.borrow());
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
            AudioRouting::RadioUsb if !usb_alive => {
                if !logged_fallback {
                    tracing::warn!("USB RX unavailable, falling back to software demod");
                    logged_fallback = true;
                }
                AudioRouting::SoftwareDemod
            }
            AudioRouting::SoftwareDemod if !demod_alive => {
                if !logged_fallback {
                    tracing::warn!("software demod unavailable, falling back to USB RX");
                    logged_fallback = true;
                }
                AudioRouting::RadioUsb
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
            _ = dsp_flags_rx.changed() => {
                dsp.set_flags(*dsp_flags_rx.borrow());
                debug!(flags = ?dsp.flags(), "audio DSP flags updated");
                continue;
            }
            block = demod_rx.recv(), if demod_alive => {
                let Some(block) = block else {
                    demod_alive = false;
                    tracing::warn!("demod audio channel closed");
                    continue;
                };
                if source != AudioRouting::SoftwareDemod {
                    continue;
                }
                dsp_scratch.clear();
                dsp_scratch.extend_from_slice(&block.samples);
                dsp.process(&mut dsp_scratch);
                // Phase 4 REC: publish post-DSP PCM for the recorder.
                // `send` returns Err only when there are no subscribers,
                // which is the common case; ignore.
                let _ = pcm_tx.send(Arc::new(dsp_scratch.clone()));
                encode_samples(&dsp_scratch, &mut frame_buf, &mut encoder, &mut seq, &tx);
            }
            block = usb_rx.recv(), if usb_alive => {
                let Some(block) = block else {
                    usb_alive = false;
                    tracing::warn!("USB RX audio channel closed");
                    continue;
                };
                if source != AudioRouting::RadioUsb {
                    continue;
                }
                dsp_scratch.clear();
                dsp_scratch.extend_from_slice(&block.samples);
                // Audio → IF demod → DSP → Audio Out.
                // IF filter is applied only to already-audio sources;
                // IQ → audio demod has its own filters upstream.
                audio_if.process(&mut dsp_scratch);
                dsp.process(&mut dsp_scratch);
                let _ = pcm_tx.send(Arc::new(dsp_scratch.clone()));
                encode_samples(&dsp_scratch, &mut frame_buf, &mut encoder, &mut seq, &tx);
            }
        }
    }
}

/// Accumulate samples into Opus frames and broadcast encoded chunks.
/// Pull the ALSA capture-device name out of a persisted
/// `StateSnapshot` when the user's last-selected device is an
/// audio-side input (FDM-DUO USB audio passthrough or a generic
/// `/proc/asound/cards` entry like a HiFiBerry HAT or USB dongle).
/// Returns `None` for IQ-side devices, empty ids, or anything that
/// doesn't look like an ALSA `hw:N,D` / `plughw:N,D` string — the
/// caller then falls back to `config.audio.rx_device`.
fn active_audio_device(snap: &StateSnapshot) -> Option<String> {
    let dev = snap.active_device.as_ref()?;
    if dev.id.is_empty() {
        return None;
    }
    match dev.kind {
        SourceKind::PortableRadio => Some(dev.id.clone()),
        SourceKind::FdmDuo if dev.id.starts_with("hw:") || dev.id.starts_with("plughw:") => {
            Some(dev.id.clone())
        }
        _ => None,
    }
}

/// Where client CAT controls are routed for a given (source, routing)
/// pair. Single source of truth consumed by both the client (for UI
/// greying) and the WS upstream handler (for command dispatch).
///
/// - `RadioUsb + FdmDuo` → `Radio` (native CAT is live, listen to the
///   radio). Any other audio-in config has no CAT surface, so `None`.
/// - `SoftwareDemod + FdmDuo` → `DemodMirrorFreq`: demod owns mode / BW
///   / filters; the radio's VFO follows the demod's center via the
///   existing tuning forwarder.
/// - `SoftwareDemod + any other IQ source` → `Demod`: the SDR has no
///   hardware radio behind it, so every control lands on the demod.
fn control_target_for(routing: AudioRouting, source: SourceKind) -> ControlTarget {
    match (routing, source) {
        (AudioRouting::RadioUsb, SourceKind::FdmDuo) => ControlTarget::Radio,
        (AudioRouting::RadioUsb, _) => ControlTarget::None,
        (AudioRouting::SoftwareDemod, SourceKind::FdmDuo) => ControlTarget::DemodMirrorFreq,
        (AudioRouting::SoftwareDemod, _) => ControlTarget::Demod,
    }
}

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
    mut cfg: efd_dsp::DrmConfig,
    drm_if_tx: broadcast::Sender<efd_dsp::AudioBlock>,
    audio_tx: mpsc::Sender<efd_dsp::AudioBlock>,
    mut mode_rx: watch::Receiver<Option<efd_proto::Mode>>,
    mut flip_rx: watch::Receiver<bool>,
    status_tx: watch::Sender<Option<efd_proto::DrmStatus>>,
    cancel: CancellationToken,
) {
    struct Active {
        cancel: CancellationToken,
        join: JoinHandle<Result<(), efd_dsp::DspError>>,
        forwarder: JoinHandle<()>,
    }
    let mut active: Option<Active> = None;
    // Flip value the currently-active bridge was spawned with; any
    // change from this demands a restart so dream picks up the new
    // `-p` arg.
    let mut active_flip: bool = *flip_rx.borrow();

    loop {
        let want_drm = matches!(*mode_rx.borrow(), Some(efd_proto::Mode::DRM));
        let want_flip = *flip_rx.borrow();
        let flip_stale = active.is_some() && active_flip != want_flip;

        // Teardown path: mode left DRM, OR flip changed on an active bridge.
        if active.is_some() && (!want_drm || flip_stale) {
            if flip_stale {
                info!(want_flip, "DRM supervisor: flip_spectrum changed, restarting bridge");
            } else {
                info!("DRM supervisor: stopping bridge");
            }
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

        if active.is_none() && want_drm {
            info!(flip = want_flip, "DRM supervisor: starting bridge");
            cfg.flip_spectrum = want_flip;
            active_flip = want_flip;
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
            r = flip_rx.changed() => {
                if r.is_err() { /* sender dropped; next loop will notice */ }
            }
        }
    }
}
