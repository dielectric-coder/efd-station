//! DRM (Digital Radio Mondiale) decoder bridge.
//!
//! Pipes a **wideband-SSB audio-IF stream** (produced by `crate::demod`
//! under `Mode::DRM`, see [`crate::demod::spawn_demod_task`]) through
//! the vendored DREAM 2.1.1 decoder over **ALSA loopback (`snd-aloop`)**.
//!
//! ```text
//! AudioBlock (48 kHz mono f32, 10 kHz BW audio-IF)
//!           ──→ alsa playback hw:Loopback,0,0
//!                                       │
//!                   dream -I plughw:Loopback,1,0 -O plughw:Loopback,0,1
//!                         --sigsrate 48000 --audsrate 48000
//!                                       │
//!                            alsa capture hw:Loopback,1,1
//!                                       │
//!                            AudioBlock → pipeline audio mpsc
//! ```
//!
//! Rationale for ALSA loopback over the previous PipeWire null-sink
//! approach: snd-aloop is a kernel module with no per-user state, so
//! the .deb runs as a dedicated `efd` user with `ProtectHome=read-only`
//! and no `XDG_RUNTIME_DIR` / linger gymnastics. The `.deb` ships a
//! `/etc/modules-load.d` snippet so the kernel auto-loads `snd-aloop`
//! at boot — true zero-config DRM.
//!
//! Rationale for audio-IF instead of raw I/Q (`-c 6`): DREAM's most-
//! tested input path is sound-card audio — the mode its built-in FLAC
//! samples exercise — so we match that format. The SSB demod upstream
//! handles decimation, VFO centering, and sideband filtering; DREAM
//! just does OFDM decode on the resulting real-valued IF signal.

use std::process::Stdio;
use std::time::Duration;

use efd_proto::DrmStatus;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::demod::AudioBlock;
use crate::error::DspError;

/// Hard cap on how long the `dream` subprocess spawn may block. Bounds
/// the bridge-setup phase so a wedged dream can't stall mode-switching.
const SUBPROC_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// snd-aloop addressing — see module docs. Subdevice 0 carries the
/// audio-IF input to DREAM; subdevice 1 carries the decoded audio out.
/// Each pair: `hw:Loopback,0,X` is the playback side, `hw:Loopback,1,X`
/// is the capture side.
///
/// `plughw:` is used for the DREAM-facing endpoints so ALSA's plug
/// layer adapts format/rate/channels if DREAM's negotiated params
/// differ slightly from ours.
const LOOPBACK_RUST_WRITE: &str = "hw:Loopback,0,0"; // Rust → DREAM (input)
const LOOPBACK_DREAM_READ: &str = "plughw:Loopback,1,0";
const LOOPBACK_DREAM_WRITE: &str = "plughw:Loopback,0,1"; // DREAM → Rust (output)
const LOOPBACK_RUST_READ: &str = "hw:Loopback,1,1";

/// Configuration for the DRM bridge.
#[derive(Debug, Clone)]
pub struct DrmConfig {
    /// Path to the vendored dream binary.
    pub dream_binary: String,
    /// Rate at which dream is driven. Must be one of 24/48/96/192 kHz
    /// (dream only accepts those). 48 kHz is the usual choice. The
    /// incoming [`AudioBlock`]s must already be at this rate — the
    /// wideband-SSB demod stage upstream is responsible for producing
    /// them at 48 kHz.
    pub dream_rate: u32,
    /// Pass `-p` to DREAM so it flips the input spectrum. Some DRM
    /// broadcasters transmit with inverted spectrum; DREAM has no
    /// automatic detection for this, so it's a manual runtime toggle
    /// (see `ClientMsg::SetDrmFlipSpectrum`).
    pub flip_spectrum: bool,
}

impl Default for DrmConfig {
    fn default() -> Self {
        Self {
            dream_binary: "dream".into(),
            dream_rate: 48_000,
            flip_spectrum: false,
        }
    }
}

/// Which input side to wire onto DREAM.
pub enum DrmInput {
    /// Production path: the bridge subscribes to a `broadcast<AudioBlock>`
    /// and writes the samples to the `hw:Loopback,0,0` snd-aloop
    /// playback. Expects the upstream Tier-1 demod to publish 48 kHz
    /// mono audio-IF.
    AudioBroadcast(broadcast::Receiver<AudioBlock>),
    /// Hardware-free test path: DREAM reads the file directly via `-f`.
    /// No Rust-side audio writer, no input-side loopback. DREAM's `-f`
    /// handler infers format from the file extension (`.flac`/`.wav`
    /// via libsndfile, `.iq`/`.if` raw s16 stereo, `.pcm` raw s16
    /// mono). Intended for `EFD_DRM_FILE_TEST` deployments and
    /// unit-test-style validation.
    File(std::path::PathBuf),
}

/// Handles returned from spawning the DRM bridge.
pub struct DrmHandles {
    /// Resolves when the bridge task exits.
    pub join: JoinHandle<Result<(), DspError>>,
    /// Live DRM decoder status, updated every time the TUI emits a frame
    /// (roughly 1 Hz). `None` until the first frame is parsed.
    pub status_rx: watch::Receiver<Option<DrmStatus>>,
}

/// Spawn the DRM bridge. The returned `DrmHandles` carries the task's
/// join handle plus a watch receiver for parsed decoder status.
///
/// On entry, this function spawns `dream` and the per-direction ALSA
/// loopback I/O tasks. On shutdown DREAM is killed and the I/O tasks
/// observe the cancel token to release their PCM handles cleanly.
pub fn spawn_drm_bridge(
    cfg: DrmConfig,
    input: DrmInput,
    audio_tx: mpsc::Sender<AudioBlock>,
    cancel: CancellationToken,
) -> DrmHandles {
    let (status_tx, status_rx) = watch::channel::<Option<DrmStatus>>(None);
    let join = tokio::spawn(
        async move { run_bridge(cfg, input, audio_tx, status_tx, cancel).await },
    );
    DrmHandles { join, status_rx }
}

async fn run_bridge(
    cfg: DrmConfig,
    input: DrmInput,
    audio_tx: mpsc::Sender<AudioBlock>,
    status_tx: watch::Sender<Option<DrmStatus>>,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    // Helper: take a Child's piped stdio handle, returning a structured
    // error on the (vanishingly unlikely) event that the kernel didn't
    // honor Stdio::piped(). `expect()` would panic the whole task.
    fn take_stdout(
        child: &mut Child,
        name: &str,
    ) -> Result<tokio::process::ChildStdout, DspError> {
        child
            .stdout
            .take()
            .ok_or_else(|| DspError::Drm(format!("{name}: stdout not piped")))
    }

    // DREAM: audio-IF input and audio output via snd-aloop.
    // No `-c 6` — we feed real-valued audio, not complex I/Q.
    // stdout is piped so the TUI parser can build a live DrmStatus.
    //
    // setsid() via pre_exec detaches DREAM from our controlling TTY (if
    // any). Without this, DREAM's curses TUI writes to /dev/tty — which
    // means in an interactive SSH session the TUI bleeds into the
    // user's terminal and our stdout pipe gets nothing, so DrmStatus
    // never reaches the client. With /dev/tty gone, DREAM's
    // open("/dev/tty") fails and the 0002-consoleio-stdout-fallback
    // patch routes the TUI to STDOUT_FILENO (our captured pipe). Under
    // systemd (no tty) this is already fine; setsid is a no-op or
    // harmless EPERM there.
    let mut dream_cmd = Command::new(&cfg.dream_binary);
    match &input {
        DrmInput::AudioBroadcast(_) => {
            dream_cmd.args([
                "-I",
                LOOPBACK_DREAM_READ,
                "-O",
                LOOPBACK_DREAM_WRITE,
            ]);
        }
        DrmInput::File(path) => {
            // `-f` disables the sound-card input entirely; DREAM opens
            // the file via libsndfile (WAV/FLAC) or as raw s16 based on
            // extension. Output still goes to the loopback so the
            // reader task can capture decoded audio.
            dream_cmd
                .arg("-f")
                .arg(path.as_os_str())
                .args(["-O", LOOPBACK_DREAM_WRITE]);
        }
    }
    dream_cmd
        .args([
            "--sigsrate",
            &cfg.dream_rate.to_string(),
            "--audsrate",
            &cfg.dream_rate.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if cfg.flip_spectrum {
        // DREAM's source takes `-p <0|1>` (GetNumericArgument), even though
        // the man page writes it as a bare flag. Passing `-p` alone makes
        // DREAM consume the next token as the argument and bail with exit 1.
        dream_cmd.args(["-p", "1"]);
    }

    // Log the exact argv we're about to exec so operators can paste it
    // into a shell and reproduce DREAM's behavior outside our pipeline.
    info!(
        dream = %cfg.dream_binary,
        args = ?dream_cmd.as_std().get_args().collect::<Vec<_>>(),
        "spawning dream"
    );
    // SAFETY: `setsid(2)` is async-signal-safe, which is the contract for
    // pre_exec (runs post-fork / pre-exec). Return value is ignored: if
    // we're already a session leader (rare — happens under some systemd
    // configurations) it returns EPERM, which is harmless because in
    // that case there's no controlling tty to detach from anyway.
    // tokio::process::Command exposes pre_exec directly on Unix — no
    // trait import needed.
    #[cfg(unix)]
    unsafe {
        dream_cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut dream = spawn_with_timeout("dream", move || dream_cmd.spawn()).await?;
    let dream_stdout = take_stdout(&mut dream, "dream")?;

    info!(
        dream = dream.id().unwrap_or(0),
        input = match &input {
            DrmInput::AudioBroadcast(_) => "audio-broadcast",
            DrmInput::File(_) => "file",
        },
        flip_spectrum = cfg.flip_spectrum,
        loopback_in = LOOPBACK_RUST_WRITE,
        loopback_out = LOOPBACK_RUST_READ,
        "DRM bridge started"
    );

    // AudioBlock → snd-aloop playback writer (only in AudioBroadcast
    // mode; File mode lets DREAM's `-f` read the file itself).
    //
    // Incoming samples are already at `dream_rate` (the demod upstream
    // handles decimation to 48 kHz), so this task just downcasts f32 →
    // mono s16le and writes via the alsa crate. ALSA writes are
    // blocking, so the work runs on the spawn_blocking pool.
    let is_file_mode = matches!(input, DrmInput::File(_));
    let writer: Option<JoinHandle<()>> = match input {
        DrmInput::AudioBroadcast(audio_rx) => {
            let cancel_w = cancel.clone();
            let rate = cfg.dream_rate;
            Some(tokio::task::spawn_blocking(move || {
                if let Err(e) = run_loopback_writer(LOOPBACK_RUST_WRITE, rate, audio_rx, cancel_w) {
                    warn!(error = %e, "DRM writer (loopback) exited with error");
                }
            }))
        }
        DrmInput::File(_) => None,
    };

    // snd-aloop capture → AudioBlock task.
    //
    // ALSA reads are blocking; runs on spawn_blocking. Frames are
    // batched 960-at-a-time (20 ms @ 48 kHz) to match the existing
    // pipeline rhythm and the AudioBlock contract.
    let cancel_r = cancel.clone();
    let audio_tx_r = audio_tx.clone();
    let dream_rate_r = cfg.dream_rate;
    let reader = tokio::task::spawn_blocking(move || {
        if let Err(e) =
            run_loopback_reader(LOOPBACK_RUST_READ, dream_rate_r, audio_tx_r, cancel_r)
        {
            warn!(error = %e, "DRM reader (loopback) exited with error");
        }
    });

    // TUI status parser task: reads dream's stdout, strips ANSI, builds a
    // DrmStatus from each frame, publishes to the watch.
    let cancel_s = cancel.clone();
    let bridge_start = std::time::Instant::now();
    let status = tokio::spawn(async move {
        let mut reader = BufReader::new(dream_stdout);
        let mut acc = DrmStatus::empty();
        let mut line = String::new();
        // Counters so operators can see at a glance whether DREAM is
        // producing TUI frames (frames_published) and whether stdout is
        // actually line-delimited (lines_read). If lines_read climbs but
        // frames_published doesn't, the frame-terminator match is off.
        let mut lines_read: u64 = 0;
        let mut frames_published: u64 = 0;
        let mut last_report = std::time::Instant::now();
        loop {
            line.clear();
            tokio::select! {
                biased;
                _ = cancel_s.cancelled() => {
                    debug!("DRM TUI: cancelled");
                    break;
                }
                res = reader.read_line(&mut line) => match res {
                    Ok(0) => {
                        debug!("DRM TUI: dream stdout EOF");
                        break;
                    }
                    Ok(_) => {
                        lines_read += 1;
                        let clean = strip_ansi(line.trim_end_matches(['\r', '\n']));
                        if parse_tui_line(&clean, &mut acc) {
                            acc.timestamp_us = bridge_start.elapsed().as_micros() as u64;
                            let _ = status_tx.send(Some(acc.clone()));
                            acc = DrmStatus::empty();
                            frames_published += 1;
                        }
                        if last_report.elapsed() >= std::time::Duration::from_secs(5) {
                            info!(
                                lines_read,
                                frames_published,
                                "DRM TUI: activity"
                            );
                            last_report = std::time::Instant::now();
                        }
                    }
                    Err(e) => {
                        warn!("DRM TUI: read error: {e}");
                        break;
                    }
                }
            }
        }
        info!(
            lines_read,
            frames_published,
            "DRM TUI: exiting"
        );
    });

    // Supervise: dream is the only subprocess now (audio I/O is
    // in-process via snd-aloop). Branch on input mode for whether a
    // clean DREAM exit is success (file mode) or failure (live mode).
    let rc: Result<(), DspError> = tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(()),
        s = dream.wait() => {
            match s {
                Ok(status) if is_file_mode && status.success() => {
                    info!("dream finished reading file");
                    Ok(())
                }
                Ok(status) if is_file_mode => {
                    warn!(?status, "dream exited non-zero (file mode)");
                    Err(DspError::Drm("dream subprocess exited non-zero".into()))
                }
                Ok(status) => {
                    warn!(?status, "dream exited unexpectedly");
                    Err(DspError::Drm("dream subprocess exited".into()))
                }
                Err(e) => Err(DspError::Drm(format!("dream wait: {e}"))),
            }
        }
    };

    // Teardown: fire the local cancel so the loopback I/O tasks exit
    // (they poll the cancel token between ALSA calls), then kill DREAM
    // if it's still alive, then await everything.
    cancel.cancel();
    let _ = dream.kill().await;
    if let Some(w) = writer {
        let _ = w.await;
    }
    let _ = reader.await;
    let _ = status.await;

    info!("DRM bridge stopped");
    rc
}

/// Open the snd-aloop playback side and stream `AudioBlock`s from the
/// upstream broadcast as mono s16le frames. Runs on `spawn_blocking`.
fn run_loopback_writer(
    device: &str,
    rate: u32,
    mut audio_rx: broadcast::Receiver<AudioBlock>,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    use alsa::pcm::{Access, Format, HwParams};
    use alsa::{Direction, PCM};

    let pcm = PCM::new(device, Direction::Playback, false)
        .map_err(|e| DspError::Drm(format!("loopback writer open {device}: {e}")))?;
    {
        let hwp = HwParams::any(&pcm)
            .map_err(|e| DspError::Drm(format!("loopback writer hwparams: {e}")))?;
        hwp.set_access(Access::RWInterleaved)
            .map_err(|e| DspError::Drm(format!("loopback writer access: {e}")))?;
        hwp.set_format(Format::s16())
            .map_err(|e| DspError::Drm(format!("loopback writer format: {e}")))?;
        hwp.set_channels(1)
            .map_err(|e| DspError::Drm(format!("loopback writer channels: {e}")))?;
        hwp.set_rate(rate, alsa::ValueOr::Nearest)
            .map_err(|e| DspError::Drm(format!("loopback writer rate: {e}")))?;
        // ~500 ms target buffer to absorb pacing jitter — same budget as
        // the previous pacat `--latency-msec=500` setting.
        let buf_frames = (rate / 2) as alsa::pcm::Frames;
        hwp.set_buffer_size_near(buf_frames)
            .map_err(|e| DspError::Drm(format!("loopback writer buffer: {e}")))?;
        pcm.hw_params(&hwp)
            .map_err(|e| DspError::Drm(format!("loopback writer apply hwparams: {e}")))?;
    }
    pcm.prepare()
        .map_err(|e| DspError::Drm(format!("loopback writer prepare: {e}")))?;

    info!(device, rate, "DRM loopback writer opened");

    // Try-recv loop with short sleeps so cancel is observed within ~20 ms.
    let mut s16: Vec<i16> = Vec::new();
    loop {
        if cancel.is_cancelled() {
            break;
        }
        match audio_rx.try_recv() {
            Ok(block) => {
                s16.clear();
                s16.extend(block.samples.iter().map(|&s| f32_to_s16(s)));
                let io = pcm
                    .io_i16()
                    .map_err(|e| DspError::Drm(format!("loopback writer io_i16: {e}")))?;
                let mut written = 0;
                while written < s16.len() {
                    match io.writei(&s16[written..]) {
                        Ok(n) => written += n,
                        Err(e) => {
                            warn!(error = %e, "loopback writer ALSA write, recovering");
                            if let Err(e2) = pcm.recover(e.errno() as i32, true) {
                                return Err(DspError::Drm(format!(
                                    "loopback writer recover: {e2}"
                                )));
                            }
                        }
                    }
                }
            }
            Err(broadcast::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                warn!(skipped = n, "DRM loopback writer: audio-IF lagged");
            }
            Err(broadcast::error::TryRecvError::Closed) => break,
        }
    }
    info!("DRM loopback writer stopped");
    Ok(())
}

/// Open the snd-aloop capture side and emit `AudioBlock`s for whatever
/// DREAM has written. Runs on `spawn_blocking`.
fn run_loopback_reader(
    device: &str,
    rate: u32,
    audio_tx: mpsc::Sender<AudioBlock>,
    cancel: CancellationToken,
) -> Result<(), DspError> {
    use alsa::pcm::{Access, Format, HwParams};
    use alsa::{Direction, PCM};

    let pcm = PCM::new(device, Direction::Capture, false)
        .map_err(|e| DspError::Drm(format!("loopback reader open {device}: {e}")))?;
    {
        let hwp = HwParams::any(&pcm)
            .map_err(|e| DspError::Drm(format!("loopback reader hwparams: {e}")))?;
        hwp.set_access(Access::RWInterleaved)
            .map_err(|e| DspError::Drm(format!("loopback reader access: {e}")))?;
        hwp.set_format(Format::s16())
            .map_err(|e| DspError::Drm(format!("loopback reader format: {e}")))?;
        hwp.set_channels(1)
            .map_err(|e| DspError::Drm(format!("loopback reader channels: {e}")))?;
        hwp.set_rate(rate, alsa::ValueOr::Nearest)
            .map_err(|e| DspError::Drm(format!("loopback reader rate: {e}")))?;
        let buf_frames = (rate / 2) as alsa::pcm::Frames;
        hwp.set_buffer_size_near(buf_frames)
            .map_err(|e| DspError::Drm(format!("loopback reader buffer: {e}")))?;
        pcm.hw_params(&hwp)
            .map_err(|e| DspError::Drm(format!("loopback reader apply hwparams: {e}")))?;
    }
    pcm.prepare()
        .map_err(|e| DspError::Drm(format!("loopback reader prepare: {e}")))?;

    info!(device, rate, "DRM loopback reader opened");

    const FRAMES_PER_CHUNK: usize = 960; // 20 ms @ 48 kHz
    let mut buf = vec![0i16; FRAMES_PER_CHUNK];
    let start = std::time::Instant::now();
    loop {
        if cancel.is_cancelled() {
            break;
        }
        let io = pcm
            .io_i16()
            .map_err(|e| DspError::Drm(format!("loopback reader io_i16: {e}")))?;
        let mut offset = 0;
        while offset < FRAMES_PER_CHUNK {
            if cancel.is_cancelled() {
                break;
            }
            match io.readi(&mut buf[offset..]) {
                Ok(n) => offset += n,
                Err(e) => {
                    warn!(error = %e, "loopback reader ALSA read, recovering");
                    if let Err(e2) = pcm.recover(e.errno() as i32, true) {
                        return Err(DspError::Drm(format!(
                            "loopback reader recover: {e2}"
                        )));
                    }
                }
            }
        }
        if cancel.is_cancelled() {
            break;
        }
        let mono: Vec<f32> = buf.iter().map(|&s| s as f32 / 32768.0).collect();
        let blk = AudioBlock {
            samples: mono,
            sample_rate: rate,
            timestamp_us: start.elapsed().as_micros() as u64,
        };
        if audio_tx.blocking_send(blk).is_err() {
            debug!("DRM loopback reader: audio_tx closed");
            break;
        }
    }
    info!("DRM loopback reader stopped");
    Ok(())
}

/// Helpers on the proto `DrmStatus` that live here because they're only
/// used by the parser.
trait DrmStatusExt {
    fn empty() -> DrmStatus;
}
impl DrmStatusExt for DrmStatus {
    fn empty() -> DrmStatus {
        DrmStatus {
            io_ok: false,
            time_ok: false,
            frame_ok: false,
            fac_ok: false,
            sdc_ok: false,
            msc_ok: false,
            if_level_db: None,
            snr_db: None,
            wmer_db: None,
            mer_db: None,
            dc_freq_hz: None,
            sample_offset_hz: None,
            doppler_hz: None,
            delay_ms: None,
            robustness_mode: None,
            bandwidth_khz: None,
            sdc_mode: None,
            msc_mode: None,
            interleaver_s: None,
            num_audio_services: 0,
            num_data_services: 0,
            timestamp_us: 0,
        }
    }
}

/// Strip ANSI CSI escape sequences (ESC [ params letter) from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // consume the final letter
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Parse `"<num> <unit>"` or `"---"` value fragments into an `Option<f32>`.
fn parse_opt_f32(s: &str) -> Option<f32> {
    let t = s.trim();
    if t.is_empty() || t.starts_with("---") {
        return None;
    }
    // Keep the leading sign, digits, dot, e/E, minus; stop at first space.
    let head: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+' || *c == 'e' || *c == 'E')
        .collect();
    head.parse::<f32>().ok()
}

fn parse_opt_u32(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.is_empty() || t.starts_with("---") {
        return None;
    }
    let head: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    head.parse::<u32>().ok()
}

/// Is the first non-whitespace character after a status label the "OK" mark?
/// dream prints 'O' for OK, ' ' for not-yet-synced.
fn flag_ok(after_colon: &str) -> bool {
    after_colon.chars().next().map(|c| c == 'O').unwrap_or(false)
}

/// Feed one line of stripped TUI text into the accumulator. Returns true
/// when the line completes a frame (i.e., it's the final "Received time -
/// date:" line), signaling the caller to publish and reset.
fn parse_tui_line(line: &str, acc: &mut DrmStatus) -> bool {
    let t = line.trim_start();

    // Status flags line: "IO:O  Time:O  Frame:O  FAC:O  SDC:O  MSC:O"
    if t.starts_with("IO:") {
        for (label, setter) in [
            ("IO:", 0_u8),
            ("Time:", 1),
            ("Frame:", 2),
            ("FAC:", 3),
            ("SDC:", 4),
            ("MSC:", 5),
        ] {
            if let Some(idx) = t.find(label) {
                let after = &t[idx + label.len()..];
                let ok = flag_ok(after);
                match setter {
                    0 => acc.io_ok = ok,
                    1 => acc.time_ok = ok,
                    2 => acc.frame_ok = ok,
                    3 => acc.fac_ok = ok,
                    4 => acc.sdc_ok = ok,
                    5 => acc.msc_ok = ok,
                    _ => {}
                }
            }
        }
        return false;
    }
    if let Some(v) = t.strip_prefix("IF Level:") {
        acc.if_level_db = parse_opt_f32(v);
        return false;
    }
    // Order matters: "MSC WMER" starts with "MSC" but isn't the MSC flag.
    if let Some(body) = t.strip_prefix("MSC WMER / MSC MER:") {
        let parts: Vec<&str> = body.split('/').collect();
        if parts.len() == 2 {
            acc.wmer_db = parse_opt_f32(parts[0]);
            acc.mer_db = parse_opt_f32(parts[1]);
        } else {
            // "---" with no slash
            acc.wmer_db = parse_opt_f32(body);
            acc.mer_db = None;
        }
        return false;
    }
    if let Some(v) = t.strip_prefix("SNR:") {
        acc.snr_db = parse_opt_f32(v);
        return false;
    }
    if let Some(v) = t.strip_prefix("DC Frequency of DRM Signal:") {
        acc.dc_freq_hz = parse_opt_f32(v);
        return false;
    }
    if let Some(v) = t.strip_prefix("Sample Frequency Offset:") {
        acc.sample_offset_hz = parse_opt_f32(v);
        return false;
    }
    if let Some(body) = t.strip_prefix("Doppler / Delay:") {
        let parts: Vec<&str> = body.split('/').collect();
        if parts.len() == 2 {
            acc.doppler_hz = parse_opt_f32(parts[0]);
            acc.delay_ms = parse_opt_f32(parts[1]);
        }
        return false;
    }
    if let Some(body) = t.strip_prefix("DRM Mode / Bandwidth:") {
        let parts: Vec<&str> = body.split('/').collect();
        if parts.len() == 2 {
            let m = parts[0].trim();
            if !m.is_empty() && !m.starts_with("---") {
                acc.robustness_mode = Some(m.to_string());
            }
            acc.bandwidth_khz = parse_opt_u32(parts[1]);
        }
        return false;
    }
    if let Some(v) = t.strip_prefix("Interleaver Depth:") {
        acc.interleaver_s = parse_opt_u32(v);
        return false;
    }
    if t.starts_with("SDC / MSC Mode:") {
        let body = &t["SDC / MSC Mode:".len()..].trim();
        if !body.is_empty() && !body.starts_with("---") {
            let parts: Vec<&str> = body.split('/').collect();
            if parts.len() == 2 {
                acc.sdc_mode = Some(parts[0].trim().to_string());
                acc.msc_mode = Some(parts[1].trim().to_string());
            }
        }
        return false;
    }
    if let Some(body) = t.strip_prefix("Number of Services:") {
        // Format: "Audio: 1 / Data: 0"
        for (label, setter) in [("Audio:", 0_u8), ("Data:", 1)] {
            if let Some(i) = body.find(label) {
                let after = &body[i + label.len()..];
                let n = parse_opt_u32(after).unwrap_or(0) as u8;
                match setter {
                    0 => acc.num_audio_services = n,
                    1 => acc.num_data_services = n,
                    _ => {}
                }
            }
        }
        return false;
    }
    // Last field in each TUI frame — signal caller to publish + reset.
    if t.starts_with("Received time - date:") {
        return true;
    }
    false
}

/// Clamp and convert an f32 (-1.0..1.0) to s16le.
#[inline]
fn f32_to_s16(v: f32) -> i16 {
    let scaled = (v * 32767.0).round();
    scaled.clamp(-32768.0, 32767.0) as i16
}

/// Run a subprocess `spawn` closure under [`SUBPROC_SETUP_TIMEOUT`].
/// `tokio::process::Command::spawn` itself isn't truly async (it's a
/// synchronous fork+exec under the hood), but wrapping it in
/// `tokio::time::timeout` lets us bound the *whole* subprocess-setup phase
/// — including the time we spend waiting on PipeWire tooling — and bail
/// out cleanly instead of hanging the bridge.
async fn spawn_with_timeout<F>(name: &'static str, f: F) -> Result<Child, DspError>
where
    F: FnOnce() -> std::io::Result<Child>,
{
    let res = tokio::time::timeout(SUBPROC_SETUP_TIMEOUT, async { f() })
        .await
        .map_err(|_| DspError::Drm(format!("{name} spawn timed out")))?;
    res.map_err(|e| DspError::Drm(format!("{name} spawn failed: {e}")))
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
    fn default_dream_rate_is_supported() {
        let cfg = DrmConfig::default();
        assert!(matches!(
            cfg.dream_rate,
            24_000 | 48_000 | 96_000 | 192_000
        ));
    }

    #[test]
    fn strip_ansi_removes_csi() {
        assert_eq!(strip_ansi("\x1b[?25lhello\x1b[H"), "hello");
        assert_eq!(strip_ansi("\x1b[K"), "");
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m end"), "red end");
    }

    #[test]
    fn parse_tui_full_frame_locked() {
        // Exact text the user pasted while decoding a real signal.
        let lines = [
            "        IO:O  Time:O  Frame:O  FAC:O  SDC:O  MSC:O",
            "                   IF Level: -17.8 dB",
            "                        SNR: 25.6 dB",
            "         MSC WMER / MSC MER: 23.6 dB / 23.6 dB",
            " DC Frequency of DRM Signal: 11965.99 Hz",
            "    Sample Frequency Offset: -5.45 Hz (-113 ppm)",
            "            Doppler / Delay: 1.11 Hz / 0.62 ms",
            "       DRM Mode / Bandwidth: B / 10 kHz",
            "          Interleaver Depth: 2 s",
            "             SDC / MSC Mode: 16-QAM / SM 64-QAM",
            "        Prot. Level (B / A): 0 / 0",
            "         Number of Services: Audio: 1 / Data: 0",
            "       Received time - date: Service not available",
        ];
        let mut acc = DrmStatus::empty();
        let mut done = false;
        for l in lines {
            done |= parse_tui_line(l, &mut acc);
        }
        assert!(done, "frame terminator should fire");
        assert!(acc.io_ok && acc.time_ok && acc.frame_ok);
        assert!(acc.fac_ok && acc.sdc_ok && acc.msc_ok);
        assert_eq!(acc.if_level_db, Some(-17.8));
        assert_eq!(acc.snr_db, Some(25.6));
        assert_eq!(acc.wmer_db, Some(23.6));
        assert_eq!(acc.mer_db, Some(23.6));
        assert!((acc.dc_freq_hz.unwrap() - 11_965.99).abs() < 0.01);
        assert!((acc.sample_offset_hz.unwrap() - (-5.45)).abs() < 0.01);
        assert_eq!(acc.doppler_hz, Some(1.11));
        assert_eq!(acc.delay_ms, Some(0.62));
        assert_eq!(acc.robustness_mode.as_deref(), Some("B"));
        assert_eq!(acc.bandwidth_khz, Some(10));
        assert_eq!(acc.interleaver_s, Some(2));
        assert_eq!(acc.sdc_mode.as_deref(), Some("16-QAM"));
        assert_eq!(acc.msc_mode.as_deref(), Some("SM 64-QAM"));
        assert_eq!(acc.num_audio_services, 1);
        assert_eq!(acc.num_data_services, 0);
    }

    #[test]
    fn parse_tui_frame_before_lock_yields_none() {
        // All fields "---" except the status flags.
        let lines = [
            "        IO:O  Time:   Frame:   FAC:   SDC:   MSC: ",
            "                   IF Level: ---",
            "                        SNR: ---",
            "         MSC WMER / MSC MER: ---",
            " DC Frequency of DRM Signal: ---",
            "    Sample Frequency Offset: ---",
            "            Doppler / Delay: ---",
            "       DRM Mode / Bandwidth: ---",
            "          Interleaver Depth: ---",
            "             SDC / MSC Mode: ---",
            "        Prot. Level (B / A): ---",
            "         Number of Services: ---",
            "       Received time - date: ---",
        ];
        let mut acc = DrmStatus::empty();
        let mut done = false;
        for l in lines {
            done |= parse_tui_line(l, &mut acc);
        }
        assert!(done);
        assert!(acc.io_ok);
        assert!(!acc.time_ok);
        assert!(!acc.frame_ok);
        assert!(acc.if_level_db.is_none());
        assert!(acc.snr_db.is_none());
        assert!(acc.robustness_mode.is_none());
        assert!(acc.bandwidth_khz.is_none());
    }

    #[test]
    fn parse_tui_handles_ansi_terminators() {
        // Real TUI lines are wrapped in \e[K etc — ensure the ANSI
        // stripper + parser handle that combination.
        let raw = "                   IF Level: -21.6 dB\x1b[K";
        let cleaned = strip_ansi(raw);
        let mut acc = DrmStatus::empty();
        parse_tui_line(&cleaned, &mut acc);
        assert_eq!(acc.if_level_db, Some(-21.6));
    }
}
