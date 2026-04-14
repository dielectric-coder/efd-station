# Changelog

All notable changes to efd-station are documented in this file.

## [Unreleased]

### Changed
- **DRM bridge simpler in file-test mode.** `spawn_drm_bridge` now takes
  a `DrmInput` enum (`AudioBroadcast` or `File`). The `File` variant
  makes DREAM read the audio-IF file directly via its `-f` flag —
  libsndfile handles WAV/FLAC natively; `.iq`/`.if` → raw s16 stereo,
  `.pcm` → raw s16 mono. No Rust-side file reader, no `drm_in` null
  sink, no `pacat` subprocess needed for the test path.
  - Deleted `server/src/drm_file_source.rs` (was ~280 LOC claxon/hound
    reader).
  - Removed `claxon` and `hound` deps from `server/Cargo.toml`.
  - `Pipeline::start_drm_file_test` no longer creates a `drm_if`
    broadcast or runs the supervisor; it spawns the bridge directly in
    `DrmInput::File` mode and winds down cleanly when DREAM hits EOF.
  - Production AudioBroadcast path untouched — live-IQ deploys behave
    identically to before.

### Added
- `DrmConfig.flip_spectrum: bool` (+ `[drm] flip_spectrum = true` in
  `config.toml`) passes `-p` to DREAM. Some DRM broadcasters transmit
  with inverted spectrum; one of DREAM's bundled FLAC samples is named
  `R_Nigeria_Mode_C_10kHz_flipped_spectrum.flac`. DREAM has no
  auto-detection for this, so it's a config toggle.

## [0.6.0] - 2026-04-13

### Changed (BREAKING — wire format)
- **WebSocket wire format now carries a version byte.** Every frame is
  prefixed with `PROTO_VERSION = 1`; receivers reject mismatched peers
  cleanly rather than producing garbled bincode state. Old clients /
  servers cannot talk to new ones — upgrade in lockstep.

### Changed (BREAKING — DRM pipeline)
- **DRM now decodes via a wideband-SSB audio-IF path, not raw I/Q.**
  - `efd-dsp::drm::spawn_drm_bridge` signature changed from
    `iq_rx: broadcast<Arc<IqBlock>>` to
    `audio_rx: broadcast<AudioBlock>`.
  - `DrmConfig` lost `iq_input_rate` and `decim_taps`.
  - DREAM is launched without `-c 6` (no complex-IQ mode); it reads the
    real-valued audio-IF stream from `drm_in.monitor` as if coming from
    a sound card — the same path its bundled FLAC samples exercise.
  - Rationale: matches DREAM's best-tested input format and mirrors the
    working `paplay FILE | dream` manual flow. The prior I/Q path was
    never validated decoding cleanly.
- **`efd-dsp::demod::spawn_demod_task` signature changed** — takes an
  additional `drm_if_tx: Option<broadcast::Sender<AudioBlock>>`. Under
  `Mode::DRM` the demod emits a 10 kHz symmetric audio-IF stream to
  this channel instead of listenable audio on `audio_tx`. The DRM
  bridge subscribes to feed DREAM.

### Added
- **`EFD_DRM_FILE_TEST` env var** — hardware-free DRM smoke test.
  When set at server startup, builds a minimal pipeline that replaces
  IQ capture + demod + CAT with a FLAC/WAV reader writing audio-IF
  samples onto the DRM input channel. The production DRM bridge, Opus
  encoder, and WS downstream run unchanged so a real `efd-client` can
  verify the full client-side chain. Exits cleanly on file EOF. See
  `DEV_GUIDE.md` §4 for the workflow and `USER_GUIDE.md` §7 for the
  operator-facing version.
- **`server/src/drm_file_source.rs`** — small claxon/hound-based reader
  that paces output at wall-clock. Supports mono 16-bit WAV and FLAC.
- **Configurable rigctld responder bind addresses.**
  `config.cat.responder_fdmduo_bind` and `responder_demod_bind` (defaults
  `127.0.0.1:4532` and `:4533`) now driven from config instead of
  hardcoded.
- **CAT input allowlist on WS upstream.** WS clients can only send CAT
  commands whose two-letter prefix is on an explicit allowlist (FA / FB /
  MD / RF / RA / LP / GT / TH / NR / NB / RT / XT / RC / RU / RD / TX /
  RX / IF / RI / SM / FR / FT / AI). Embedded `;` rejected to prevent
  command smuggling.
- **Real ATT / LP / NR / NB / AGC state queries** in `efd-cat`'s poll —
  `RA;` / `LP;` / `NR;` / `NB;` / `GT;` responses parsed and merged
  into `RadioState` (were hardcoded defaults before).
- **DRM bridge robustness** — 5 s subprocess setup timeouts (pactl /
  pacat / parec / dream), post-lag audio-resync purge so stale audio
  doesn't blend into fresh audio after an IQ gap, non-blocking
  `NullSinks::drop`, `setsid()` on DREAM spawn to detach from the
  controlling TTY (required for the TUI parser to capture DrmStatus
  from under an interactive SSH session).
- **Graceful server shutdown** — bounded `pipeline.shutdown()`
  (5 s cap), structured `run() -> Result<…>` in `main.rs` so bind /
  signal-handler failures surface with a log instead of panic.

### Fixed
- Client audio ring buffer grown from 200 ms to 1.5 s. Absorbs DREAM's
  frame-aligned ~400–500 ms output bursts without dropping samples,
  eliminating the periodic "1 s on / 1 s off" chopping that appeared
  with the new DRM pipeline.

### Runtime dep (not a source change)
- **DRM decoding now requires `libfaad2` installed on the CM5**
  (`sudo apt install libfaad2`). DREAM dlopens it at runtime to decode
  AAC audio. Without it the OFDM layer still locks fine (all flags `O`,
  MSC:O, good SNR) but produces silence — watch for the
  `No usable FAAD2 aac decoder library found` line in the server log.

### Docs
- `CLAUDE.md`, `README.md`, `USER_GUIDE.md` updated to reflect the new
  DRM pipeline (wideband-SSB demod → audio-IF → DREAM sound-card mode).
- `DEV_GUIDE.md` adds the `EFD_DRM_FILE_TEST` hardware-free dev loop
  and DRM bridge diagnostic-log guidance.

## [0.5.0] - skipped

Version number burned by a reset-then-deployed binary (a `--version`
flag commit was built and dpkg'd to the CM5, then the commit was reset
locally but the binary stayed installed). Jumped past it to avoid
confusion with the phantom deployed build. See memory note
`feedback_deployed_vs_source_skew.md`.

## [Pre-0.6.0 docs work]

### Docs
- New `USER_GUIDE.md` — end-user walkthrough: hardware setup, install,
  configuration, UI tour, operating modes (including DRM prerequisites on
  the CM5), and troubleshooting.
- New `DEV_GUIDE.md` — developer walkthrough: repo tour, build/test/run,
  extension recipes (new IQ driver, analog demod mode, audio decoder,
  rigctld command, proto field), deployment tooling, conventions.
- `CLAUDE.md` remains the authoritative architectural reference; the two
  guides point at it instead of duplicating.

## [0.4.3] - 2026-04-12

### Fixed
- DRM bridge: run efd-server under the login user (`mikel`) instead of the
  dedicated `efd` system user so the bridge can reach the per-user PipeWire
  socket at `/run/user/1000/pulse/native`. The old setup caused
  `pactl load-module: No such file or directory` (no pactl) and, after
  installing `pulseaudio-utils`, would still have failed to connect to a
  PipeWire daemon — PipeWire is a per-user session and the `efd` system user
  had none.
- `dist/systemd/efd-server.service`: `User=mikel`, `XDG_RUNTIME_DIR` set
  explicitly, `ReadWritePaths` updated to `/home/mikel/.config/efd-backend`.

### Added
- `scripts/migrate-service-to-mikel.sh` — idempotent one-shot migration on
  the CM5: enables linger for mikel, adds the dialout/audio/plugdev groups,
  copies the config from `/home/efd` to `/home/mikel`, installs the new unit,
  sanity-checks `pactl info` as the target user, restarts the service.

## [0.4.2] - 2026-04-12

### Changed
- efd-iq reshaped into the driver-per-device layout prescribed by CLAUDE.md:
  flat `backend.rs`/`device.rs`/`stream.rs` replaced by `source.rs` + `types.rs`
  + `drivers/fdm_duo.rs`. Public surface preserved (`IqBlock`, `SourceConfig`,
  `FdmDuoConfig`, `spawn_source` all re-exported from the crate root).
- `rusb` is now an optional dependency behind a new `fdm-duo` cargo feature
  (default-on). Building `efd-iq` with `--no-default-features` produces a
  driver-less crate — the scaffolding for HackRF/RSPdx/RTL drivers to slot in
  behind their own feature flags.
- `IqError::Usb` is gated behind `feature = "fdm-duo"`.
- `efd_iq::device::*` → `efd_iq::drivers::fdm_duo::*` (one consumer updated:
  `server/src/bin/agc_experiment.rs`).

No runtime behaviour change; no API change for server or `efd-dsp`.

## [0.4.1] - 2026-04-10

### Fixed
- CAT serial: validate response prefix matches command sent, discard stale
  responses from previous commands (was reading SM/RI as IF responses)
- CAT poll checks cancel between serial commands for faster shutdown
- update-pi.sh: stop old server before installing new one (USB Resource busy)
- Waterfall: pixel buffer rendering with safe surface creation (no more unsafe)

## [0.4.0] - 2026-04-10

### Security
- Bounded client message queue (max 256) — prevents OOM if GTK can't keep up
- Bounded waterfall pending buffer (max 64 lines)
- WS downstream send timeout (2s) — disconnects slow clients instead of blocking
- Eliminated unsafe `create_for_data_unsafe` in waterfall — now clones pixel data safely

### Fixed
- WS handler now aborts the other task on client disconnect (was leaking tasks + broadcast receivers)
- Simplified server shutdown: cancel + 3s wait (removed fragile `Arc::try_unwrap` logic)
- FM demod output normalized to [-1.0, 1.0] based on 5kHz max deviation (was raw radians)
- Demod lag warning now logs estimated milliseconds of dropped audio
- Mutex poison recovery throughout client (unwrap_or_else + into_inner)

### Changed
- Spectrum grid drawn with single Cairo stroke (was 18 per frame)
- Controls bar caches previous state, skips redundant GTK widget updates
- WS client reconnect uses exponential backoff (2s → 30s cap)

## [0.3.1] - 2026-04-10

### Added
- GTK4 client application with spectrum, waterfall, and controls
  - Cairo spectrum display (magnitude vs frequency, dB grid)
  - Scrolling waterfall spectrogram (blue→cyan→green→yellow→red palette)
  - Controls bar: frequency, mode, VFO, BW, S-meter bar, TX indicator
  - PTT toggle button
  - WS auto-reconnect on disconnect
- Headless WS test client (`cargo run --example ws_test`)
  - Connects, decodes all message types, prints rate stats for 10s
  - Validates full server pipeline end-to-end
- RI (RSSI) command support — reads signal strength in dBm directly
- S-meter parsing corrected to match FDM-DUO manual scale
  (0000=S0, 0011=S9, 0022=S9+60)

### Fixed
- S-meter now shows live dBm values (was stuck at -127)
- Poll tries RI; first (dBm), falls back to SM0; (S-units)

### Verified on hardware
- CM5 + FDM-DUO: FFT 15.6/s, RadioState 2.5/s, Audio 50.0/s
- S-meter reading within ~5dB of front panel display
- Ctrl-C clean shutdown working
- CAT serial auto-discovery working (sysfs hub-sibling)

## [0.3.0] - 2026-04-10

### Security
- Add bincode size limit (4 KB) on WS frame decode to prevent OOM from malicious clients
- Validate CAT commands from WS clients: length limit, printable ASCII only, must end with `;`
- Add TX audio frame size limit (2 KB)

### Fixed
- Replace unsafe `File::from_raw_fd` + `mem::forget` in serial.rs with safe `nix::unistd::read/write`
- Handle mutex poison gracefully in CAT poll/command tasks (recover instead of panic)
- Proper graceful shutdown via `Pipeline::shutdown()` with 3s timeout instead of immediate `process::exit(0)`

### Added
- S-meter polling via `SM0;` CAT command — `RadioState.s_meter_db` now has live readings
- TX state extracted from IF response — `RadioState.tx` reflects actual transmit status
- Separate `audio.tx_device` config field for USB TX audio output
- New `parse_sm_response()` and `IfResponse` struct in parse module

### Changed
- efd-dsp now depends on efd-iq directly, using `efd_iq::IqBlock` as the single source of truth
- Removed IQ forwarder task — eliminated per-block sample Vec clone
- FFT `center_freq_hz` initialized to 0 (clients use `RadioState.freq_hz` for display)

## [0.2.0] - 2026-04-10

### Added
- Direct serial CAT control (38400 8N1) — talks to FDM-DUO FTDI port directly
- Auto-discovery of CAT serial port via sysfs hub-sibling scanning
- Udev rule creating `/dev/fdm-duo-cat` symlink

### Removed
- rigctld dependency — no longer needed
- hamlib-utils from package dependencies

### Changed
- Simplified `[cat]` config: just `serial_device` and `poll_interval_ms`

## [0.1.0] - 2026-04-09

### Added
- Initial release — complete backend implementation
- **efd-proto**: shared WS message types with bincode serialization
  - `ServerMsg`: FftBins, AudioChunk, RadioState, Error
  - `ClientMsg`: CatCommand, TxAudio, Ptt
- **efd-iq**: USB IQ capture from FDM-DUO (rusb, 192 kHz, 32-bit IQ)
  - FIFO init sequence ported from EladSpectrum
  - Auto-discover by USB VID:PID (1721:061a)
- **efd-dsp**: FFT processing + demodulation
  - 4096-point FFT with Blackman-Harris window and 3-frame averaging (rustfft)
  - AM, USB, LSB, FM demodulators with 192k→48k decimation
- **efd-audio**: ALSA playback, Opus codec, USB TX audio
  - Opus wideband 48 kHz encode/decode (20ms frames)
  - ALSA HAT output with configurable latency
  - USB TX audio path for client-originated transmit
- **efd-cat**: rigctld TCP client with Kenwood CAT parsing
  - IF response parsing (frequency, mode, VFO)
  - RF response parsing (filter bandwidth with lookup tables)
  - Periodic state polling (200ms default)
- **server**: Axum HTTP/WS server
  - Full tokio pipeline: IQ → FFT → WS, IQ → demod → Opus → WS/ALSA
  - Per-client WS handler with downstream (broadcast→bincode→WS) and upstream (WS→bincode→mpsc)
  - TOML config at `~/.config/efd-backend/config.toml`
  - `/health` endpoint, `/ws` WebSocket endpoint
  - Graceful shutdown on SIGINT/SIGTERM
- Packaging: .deb (cargo-deb) and Arch/Manjaro (PKGBUILD)
- Systemd service with dedicated `efd` user
