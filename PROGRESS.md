# Progress

## Completed

### Phase 0: Workspace scaffolding
- Cargo workspace with 6 crates, all compiling
- `.gitignore`, workspace dependency sharing

### Phase 1: efd-proto — shared message types
- All WS message types: FftBins, AudioChunk, RadioState, ErrorMsg, CatCommand, TxAudio, Ptt
- ServerMsg / ClientMsg tagged enums for WS framing
- Bincode round-trip tests (9 tests)

### Phase 2: efd-iq — USB IQ capture
- FDM-DUO USB device open, FIFO init, bulk read (ported from EladSpectrum C)
- i32 LE → f32 normalized sample conversion
- `spawn_iq_capture()` tokio blocking task publishing `Arc<IqBlock>` on broadcast channel
- Auto-discover by VID:PID (1721:061a)

### Phase 3: efd-dsp — FFT + demodulation
- 4096-point FFT with Blackman-Harris window and 3-frame averaging (rustfft)
- AM (envelope), USB/LSB (real part), FM (phase differencing) demodulators
- 192k → 48k decimation
- `spawn_fft_task()` and `spawn_demod_task()`

### Phase 4: efd-cat — CAT control
- Direct serial port (38400 8N1) — no rigctld dependency
- IF; response parsing (freq, mode, VFO, TX state)
- RF; response parsing (filter bandwidth with full lookup tables)
- SM0; S-meter polling
- Auto-discovery: udev symlink → /dev/serial/by-id/ → sysfs hub-sibling scan
- Mutex-shared serial port between poll and command tasks
- Input validation on CAT commands from WS clients

### Phase 5: Server config + Axum skeleton
- TOML config with defaults (`~/.config/efd-backend/config.toml`)
- Axum HTTP server with `/health` and `/ws` routes
- SIGINT/SIGTERM graceful shutdown

### Phase 6: Pipeline wiring
- All broadcast/mpsc channels created and wired
- IQ → FFT, IQ → demod → Opus → broadcast, CAT poll → broadcast
- ALSA bridge (Opus decode → PCM → ALSA)
- USB TX audio path

### Phase 7: WebSocket downstream + upstream
- Per-client task pairs (sender + receiver)
- Downstream: subscribe broadcasts → bincode → WS binary frames
- Upstream: WS binary → bincode decode (with 4KB size limit) → route to mpsc
- CAT command validation, TX audio size limit

### Phase 8: Demod + ALSA + Opus
- AM/USB/LSB/FM demodulators with decimation
- Opus wideband 48kHz encode/decode
- ALSA HAT playback with configurable latency

### Phase 9: TX audio path
- Opus decode from WS → ALSA write to FDM-DUO USB audio
- Separate `tx_device` config

### Code review fixes (v0.3.0)
- Eliminated unsafe serial fd handling
- Added bincode size limits (DoS prevention)
- CAT command validation (length, charset, terminator)
- Proper graceful shutdown with timeout
- Mutex poison recovery
- Live S-meter and TX state in RadioState
- Eliminated IqBlock clone (efd-dsp uses efd-iq type directly)

### Packaging
- .deb via cargo-deb (CM5 / Raspberry Pi OS)
- PKGBUILD for Arch/Manjaro
- Systemd service, udev rules, example config
- Dedicated `efd` system user with dialout/audio/plugdev groups

---

## Suggested next steps

### Client UI (GTK4)
- [ ] GTK4 + GtkGLArea client application
- [ ] Spectrum display (OpenGL, receives FftBins)
- [ ] Waterfall display (scrolling spectrogram)
- [ ] VFO controls (frequency, mode, BW, step)
- [ ] S-meter / power meter display
- [ ] PTT button (spacebar shortcut)
- [ ] RIT/XIT controls
- [ ] Memory store/recall
- [ ] Audio playback (Opus decode → PipeWire)

### CAT completeness
- [ ] Poll ATT, LP, AGC, NR, NB state (pending identification of correct CAT commands for FDM-DUO)
- [ ] Parse AGC mode from radio response
- [ ] Frequency set command from client (FA command)
- [ ] Mode change from client (MD command)

### Audio improvements
- [ ] Discover FDM-DUO USB audio device automatically (similar to CAT serial discovery)
- [ ] Separate RX/TX audio device auto-configuration
- [ ] Audio AGC (software, before ALSA output)
- [ ] Noise reduction (LMS or spectral subtraction)

### DSP improvements
- [ ] CW decoder (Goertzel or matched filter)
- [ ] DREAM integration for DRM decoding
- [ ] FreeDV codec integration
- [ ] Variable FFT size from config
- [ ] Dynamic center frequency update from RadioState

### Network / security
- [ ] TLS support (wss://) for remote operation
- [ ] API key or token authentication
- [ ] Per-client rate limiting
- [ ] Client connection management (max clients, kick, ban)

### Robustness
- [ ] USB device reconnection on disconnect/replug
- [ ] Serial port reconnection on CAT errors
- [ ] Watchdog: restart tasks that crash
- [ ] Health metrics endpoint (task status, buffer fill levels, dropped frames)

### Digital modes
- [ ] PTT arbitration (when WSJT-X and client both have PTT capability)
- [ ] CAT sharing — expose CAT via TCP server so WSJT-X can connect
- [ ] Virtual audio device support for digital mode apps on client side

### Packaging / CI
- [ ] GitHub Actions CI (build + test on push)
- [ ] Cross-compilation for aarch64 from x86_64 host
- [ ] Release binaries (GitHub Releases)
- [ ] AUR package submission
