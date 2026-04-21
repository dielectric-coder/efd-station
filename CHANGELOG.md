# Changelog

All notable changes to efd-station are documented in this file.

## [Unreleased]

### Fixed (server 0.10.12 / client 0.8.6 — GS P2 is 3 digits, not 2)
- The manual's §6.3.2 GS table shows only two P2 cells, but the
  radio's own reply to `GS0;` is `GS0001;` — a 4-byte payload
  (`P1` + three-digit `P2`). The 0.10.9 wire format was one byte
  short, so the radio silently rejected every `GS`, which is why
  toggling Slow/Medium/Fast produced no behavioural change even
  though the command reached the radio cleanly.
- Client `set_agc_mode` now emits `GS0000;` / `GS0001;` / `GS0002;`.
- Server `parse_gs_response` expects a 4-byte payload; parser /
  tests / `gs_to_agc_mode` widened from `u8` to `u16` so the manual
  gain range (`000`..`010`) also round-trips cleanly.
- Bench-verified against firmware Rev 2.13.

### Diag (server 0.10.11 — log the AGC poll replies too)
- Set-command logs alone can't tell us whether the radio is
  accepting `GC`/`GS` — TH echoes empty too and that one definitely
  works. Promoted the poll-path `GC;` and `GS<P1>;` reads to info
  with the raw response, so the journal shows what the radio
  actually answers with. Non-functional otherwise.

### Fixed (server 0.10.10 — GS poll needed P1)
- Manual §6.3.2 shows `GS` Read format as `GS<P1>;` — a bare `GS;`
  is invalid and the radio doesn't return a parseable answer. The
  0.10.9 poll path was sending `GS;`, so `parse_gs_response` always
  failed and the server fell through to `AgcMode::Slow` regardless
  of reality. The tile label never matched the radio, which looked
  like "GC/GS don't work" even if the Set commands themselves took.
- Poll now reads `GC;` first, then `GS0;` (auto) or `GS1;` (manual)
  depending on `GC`'s answer.
- Temporary diagnostic: AGC-plane CAT commands (`GC`/`GS`/`TH`) are
  logged at `info` level with their radio response, so the wire
  exchange is visible in the journal without flipping the whole
  crate to debug. Dropped back to debug once confirmed stable.

### Fixed (server 0.10.9 / client 0.8.5 — use native GC/GS for AGC)
- **AGC speed never reached the radio.** The previous implementation
  emitted `GTnnn;` (Kenwood-compat), but §6.3.3 of the FDM-DUO manual
  lists `GT` as a compatibility no-op ("no effect on the transceiver").
  The native AGC surface is `GC` (auto vs manual gain) + `GS` (speed
  or manual gain value), per §6.3.2 pages 48–49.
- Client `cat_commands::set_agc_mode` now returns a `Vec<CatCommand>`:
  `Off` emits `GC1;` (manual gain, AGC bypassed); Slow/Medium/Fast
  emit `GC0;` followed by `GS000;` / `GS001;` / `GS002;`.
- AGC dropdown gains an `Off` entry (preselect order: Off/Slow/Med/Fast).
- Server `efd-cat` polling reads `GC;` + `GS;` instead of `GT;`;
  `parse_gt_response` removed, `parse_gc_response` / `parse_gs_response`
  / `gs_to_agc_mode` added. 5 new parse tests replace the old GT test.
- `ws::upstream` allowlist: `GT` removed (useless), `GC` + `GS` added.

### Added (client 0.8.4 — AGC popup: slider + speed dropdown)
- AGC chip tile now opens a modal editor with a 0–10 threshold slider
  (tick-marked at every step) and a Slow / Medium / Fast dropdown.
  Both values commit on OK; Cancel discards. Tile label renders as
  `agc <n> S|M|F` to show the current state at a glance.
- New `cat_commands::set_agc_mode(AgcMode)` emits `GTnnn;` with the
  FDM-DUO's 0–20 bucket mapping (Off=0, Fast=4, Medium=11, Slow=17)
  — the same bands `efd-cat::parse::parse_gt_response` decodes from
  the radio.
- `SdrParams` gains an `agc_speed` field (serde-default `"slow"` so
  existing on-disk params load cleanly), persisted on quit alongside
  threshold.
- `apply_capabilities` now pushes both the threshold AND the speed to
  the radio on connect (Radio target only); `sync_from_radio`
  refreshes the tile from the polled `RadioState.agc` +
  `agc_threshold` so the label tracks hardware knob twists.

### Added (server 0.10.8 — ControlTarget routing)
- New `efd_proto::ControlTarget` enum on `Capabilities`
  (`None | Radio | Demod | DemodMirrorFreq`) — computed server-side
  from (AudioRouting × SourceKind) and used as the single source of
  truth for both client-side greying and server-side CAT routing.
- Server (`ws::upstream`) now routes each `CatCommand` through
  `cat_route_for`: `None` drops everything, `Radio` forwards all
  (today's behavior), `DemodMirrorFreq` forwards only `FA`/`FB`
  (freq) to the radio so the existing tuning forwarder propagates
  the new center to the demod, and `Demod` drops all (the runtime
  SDR retune channel lands when non-FDM-DUO IQ drivers do).
- `ClientMsg::Ptt` is now gated to `Radio`/`DemodMirrorFreq`; in
  `Demod`/`None` PTT is dropped with a debug log.
- Client (`ControlBar::apply_capabilities`) greys PTT, freq tile,
  AGC tile, mode dropdown, and the per-mode toggle buttons when
  `control_target == None`. The initial AGC-threshold sync-on-
  connect is restricted to `Radio` (was `has_hardware_cat`) so IQ
  + FDM-DUO sessions don't push a TH command that the server would
  drop.
- 4 new routing tests in `ws::upstream::tests`.

### Added (server 0.9.1 — phase 3c: DNR / DNF / APF filters go live)
- **`DNR`** (`efd_dsp::audio_dsp`) — 2-pole IIR lowpass at 2.5 kHz,
  Butterworth Q. Voice sits at 300–2800 Hz; HF hiss lives above.
  Chop the hiss, keep voice intelligible. Not spectral-subtraction
  "noise reduction" in the literature sense, but the effect HF
  operators actually ask for under the DNR label, and zero
  artifacts (no FFT smearing, no noise-profile learning glitch).
- **`DNF`** — narrow biquad notch at 1 kHz, Q=15. Targets the
  classic single-tone heterodyne whistle. A future phase wires
  this to a user-settable centre (most SDR apps expose a draggable
  notch); 1 kHz is a sensible default for casual HF.
- **`APF`** — RBJ peaking EQ at 700 Hz, Q=3, +6 dB. Lifts the CW
  sidetone / voice-fundamental band so weak signals stand out.
- All three share an RBJ-cookbook `Biquad` (copied from
  `audio_if.rs` — when a second shared consumer appears, promote
  to a dedicated module). State resets on toggle-on so stale
  history doesn't ring.
- **6 new unit tests** in `audio_dsp.rs` covering the three
  filters' pass/reject characteristics at voice-band and
  off-centre frequencies. The pre-existing DNB tests (phase 3b)
  all still pass.
- **Flag flip resets filter state**. Toggling a filter on after
  it's been off resets its x1/x2/y1/y2 history so the transient
  sample boundary doesn't audibly click.

### Not in this commit (future phase)
- **True spectral-subtraction DNR** (FFT-based, noise-profile
  learning). The current lowpass is the pragmatic first-pass;
  spectral subtraction can land as a `DnrMode` proto field when
  the UI gains a selector.
- **User-tunable notch / peak centres**. Both DNF and APF are
  fixed today; a future phase adds click-to-tune or slider UI
  plus matching `ClientMsg::SetDnfCentre` / `SetApfCentre`.
- **Adaptive tone tracking** for DNF (auto-move the notch to
  whatever tone is currently whining).

### Added (client 0.8.3 — phase 5d: WSJT-X launcher + dynamic device chips + status line)
- **WSJT-X launcher** — `ctrl0-right` button now actually spawns
  `wsjtx` as a detached child process. Stdio is redirected to
  `/dev/null` so the GUI doesn't get lost; the `Child` handle is
  dropped (fire-and-forget). Missing binary / fork failure is
  logged at `error!` level; the button stays pressable. WSJT-X
  itself needs to be pointed at `localhost:4532` — set up a
  `ssh -L 4532:localhost:4532 pi@cm5` tunnel per the README.
- **Dynamic device chips** in `disp1-left`. The hard-coded
  `FDM` / `HRF` pair is gone; the cell now rebuilds on every
  `ServerMsg::DeviceList` push, adding one chip per discovered
  device with a three-letter abbreviation (`FDM` / `HRF` / `RSP`
  / `RTL` / `POR` / `AF` / `IQF`). Active kind gets
  `.chip-active`, the rest `.chip-inactive`. Tooltips carry the
  full `DeviceId { kind, id }`. Empty-id synthetic file-source
  placeholders are filtered out.
- **Non-DRM status line** in `disp1-center`. When the current
  mode isn't `DRM`, the cell shows `SNR <db> dB   NB  DNR off
  DNF off APF off   decode <list>`. Toggle-is-off tags dim via
  Pango `alpha='45%'` so the on-state pops. SNR pulls from
  `RadioState.snr_db` (server fills in phase 3+). Decoder list
  pulls from `snapshot.enabled_decoders`. `drm_line1` and the
  new `status_line` are mutually-visible — the current mode
  picks which.
- **`DisplayBar::set_dsp_status`** new method called from
  `ControlBar::apply_snapshot` so the cell stays in sync with
  the button states.
- **`DisplayBar::set_device_list`** new method called from
  `main.rs`'s WS dispatch on `ServerMsg::DeviceList`. Replaces
  the `eprintln!` debug.

### Changed (client 0.8.2 — phase 5c: display-bar layout matches drawio IQ-NO-DRM)
- **Unified tuning line** in `disp0-center`. The split
  vfo/freq/mode/bw/S-meter cluster collapses to a single Pango-
  markup label rendering
  `f 14 200 000 Hz   demod CWᵤ   bw 2.4 kHz   RIT +0 Hz   IF +0 Hz`
  from the latest `RadioState`. Freq uses three-digit grouping with
  spaces (matches drawio) rather than dots. Mode subscripts
  (CWᵤ / CWₗ / SAMᵤ / SAMₗ / FMₙ) render via
  Pango `<sub>` tags.
- **Source-class chips** in `disp0-left`: two blue pills, `AUD`
  and `IQ`. Active class gets `.chip-active` (bright blue); other
  class is `.chip-inactive` (dim blue) when available or
  `.chip-disabled` (grey) when the server doesn't advertise it.
- **Device chips** in `disp1-left`: `FDM` and `HRF`. Only the
  driver-backed kind lights up — HRF stays `.chip-disabled`
  today (no HackRF driver in `efd-iq` yet).
- **Active-source pill** in `disp2-left` (green `chip-source`)
  showing e.g. `FdmDuo IQ` from the server's `Capabilities`.
- **Audio-routing indicator** in `disp2-right` (grey
  `chip-passthrough` pill): `PASSTHROUGH` when the radio's USB
  audio goes straight out, `SWDEMOD` when the IQ chain produces
  audio.
- **S-meter relocated** to `disp1-right` per the drawio (was
  inline with tuning text in disp0-center). S-meter bar gets a
  wider, taller rendering.
- **dBm readout** added to `disp0-right` next to the RX/TX pill.
- **Pill CSS palette** in the client's stylesheet:
  `.chip-active` / `.chip-inactive` / `.chip-disabled` for
  source+device chips, `.chip-source` green for the live-source
  pill, `.chip-passthrough` grey for the audio-routing pill.
- **`DisplayBar::set_active_device`**, **`set_active_source_label`**,
  **`set_passthrough`** new methods, called from
  `ControlBar::apply_capabilities` so the server's `Capabilities`
  drives the chip state.
- **`DisplayBar::set_freq_immediate`** re-renders the tuning line
  from the cached state instead of writing a dedicated freq label,
  so optimistic typed frequencies still look right in the unified
  layout.

### Not in this commit (phase 5d targets)
- **SRC / DEV** dedicated buttons in `ctrl0-left` and click-to-
  tune yellow chips (`f`, `bw`, `rit`, `IF`) in `ctrl0-center`.
  Current tune-entry + step-dropdown + ± buttons still provide
  the functionality, just not in the chip-styled form.
- **Real WSJT-X launcher** + **CONFIG dialog** (placeholders
  today).
- **Dynamic device list** — `disp1-left` hard-codes `FDM` / `HRF`;
  future phase reads `DeviceList` and shows the real enumeration.
- **SNR / DSP-status text** in `disp1-center` when not in DRM
  mode. `drm_line1` is reused but only populated under
  `Mode::DRM`.

### Changed (client 0.8.1 — phase 5b: control-bar layout matches drawio IQ-NO-DRM)
- **Control-bar rows reshuffled** to match
  `docs/client-sdr-UI.drawio` layer `IQ-NO-DRM`:
  - `ctrl0-left` — (unchanged, SRC toggle + PTT / Mute / Volume
    in center)
  - `ctrl0-center` — tune controls (freq entry, step, ± buttons)
    + DRM flip toggle move here from ctrl1. The old
    mode-dropdown is hidden but kept in the widget tree to
    preserve test/id references.
  - `ctrl0-right` — new **WSJT-X** launcher button (placeholder;
    click logs a TODO, real launcher is phase 5c).
  - `ctrl1-left` — `NB` + `APF` toggles (moved from the old
    ctrl2-left block).
  - `ctrl1-center` — **IF-demod mode buttons** replace the
    dropdown: `AM / SAM / DSB / USB / LSB / CWᵤ / CWₗ / FMₙ`.
    Linked radio group; exactly one stays active. Each click
    sends `SetDemodMode` + the matching CAT `MD…;`. Orange
    styling per the drawio.
  - `ctrl1-right` — `REC` toggle (moved from ctrl2-right).
  - `ctrl2-left` — `DNR` + `DNF` toggles (moved from ctrl2-left's
    old one-row block).
  - `ctrl2-center` — **decoder toggles**: `CW / PSK / MFSK /
    RTTY / FAX / PCKT` (purple, audio-domain) + `DRM / FDV`
    (pink, digital voice). Independent toggles; multiple can be
    active. Server-side decoders aren't wired up yet so clicks
    emit `SetDecoder` messages the server debug-logs and
    ignores — the buttons are here for layout fidelity and to
    exercise the phase-1 proto path.
  - `ctrl2-right` — new **CONFIG** button (placeholder; click
    logs a TODO, real settings dialog is phase 5c).
- **`DNB` button removed** from the UI per the drawio. The
  `AudioDspFlags.dnb` field on the wire and in the pipeline is
  unchanged — the server still honours the flag when it's set
  via the persisted snapshot — but normal operator flow uses the
  pre-IF `NB` only, per the diagram.
- **CSS palette added** matching the drawio chips: `.dsp-toggle`
  / `.chrome-btn` yellow (`#fff2cc`), `.mode-btn` orange
  (`#f0a30a`), `.decoder-audio` purple (`#e1d5e7`),
  `.decoder-drm` pink (`#f8cecc`). Pressed states get a
  darker shade of the same hue.
- **`apply_snapshot` extended** to seed the mode buttons and
  the decoder toggles from the persisted snapshot so
  `snapshot.mode` / `snapshot.enabled_decoders` come up correctly
  after reconnect.

### Not in this commit (drawio gaps remaining for phase 5c)
- **Unified tuning line** in `disp0-center` (`f <big freq>
  demod <mode> bw <w> RIT <r> IF <i>`). The current split
  labels still show the same data, just not in a single
  markup line.
- **Source / device chips** in `disp0-left` / `disp1-left` —
  `AUD`/`IQ` and `FDM`/`HRF` blue pills from the drawio.
  Current label-based availability indicator stays for now.
- **`disp2-left` current-source pill** (`FDM IQ` green badge),
  **`disp2-right` PASSTHROUGH pill**, **`disp1-right` S-meter
  restyling**.
- **SRC / DEV** buttons in `ctrl0-left` and click-to-tune
  yellow chips (`f`, `bw`, `rit`, `IF`) in `ctrl0-center`.
- **WSJT-X launcher + CONFIG dialog** — the button shells are
  in place; their real behaviour ships in phase 5c.

### Added (client 0.8.0 — phase 5a: DSP toggles + REC + decoded-text)
- **DSP toggle row** in the client's `ctrl2-left` cell — five
  `ToggleButton`s for `NB` / `DNB` / `DNR` / `DNF` / `APF`. `NB`
  drives `ClientMsg::SetNb` (pre-IF, phase 3a/b, real blanker).
  `DNB` drives `ClientMsg::SetDnb` (audio-domain, phase 3b, real
  blanker). `DNR` / `DNF` / `APF` drive their matching messages;
  the pipeline wires the flags through (phase 3a) but the filter
  math is phase 3c, so those three are currently
  click-has-no-audible-effect stubs. Tooltips spell this out.
- **REC button** in `ctrl2-right` with a status label next to it.
  Active state mirrors the server-authoritative
  `ServerMsg::RecordingStatus`. Click sends `StartRecording`
  (audio kind by default) / `StopRecording`. While active, the
  status label shows kind / duration / KiB written, updated by
  the server's ~1 Hz status push.
- **Decoded-text area** in `disp2-center` — rolling 6-line log of
  the last audio-domain decoder outputs
  (`ServerMsg::DecodedText`). Each line tagged with decoder kind
  so parallel decoders stay scannable. Widget is empty today
  because no Tier-3 decoders are wired up server-side yet; the
  plumbing is in place for when they land.
- **`ControlBar::apply_snapshot`** — seeds all five DSP toggles
  from the connect-time `ServerMsg::StateSnapshot` push so
  persisted preferences ("always start with DNR on") show up
  correctly. `suppress_toggle_notify` gates the sync so
  `set_active` doesn't bounce back as a `SetNb` / `SetDnb` / etc.
  to the server.
- **`ControlBar::apply_rec_status`** — mirrors server recording
  state into the REC button + status label, with the same
  suppress-notify guard.

### Deliberately deferred (phase 5b+)
- **Source / device pickers.** `disp0-left` still shows the
  existing AUD/IQ toggle only; there's no dropdown for selecting
  among discovered devices yet. `ServerMsg::DeviceList` is still
  printed to stderr. Phase 5b adds the picker UI and wires
  `ClientMsg::SelectDevice`.
- **Mode buttons** (AM / SAM / USB / LSB / CWᵤ / CWₗ / FMₙ)
  replacing the current mode dropdown. No urgency — the dropdown
  does the same thing.
- **Decoder-selection buttons** (PSK / CW / MFSK / RTTY / FAX /
  PCKT / DRM / FDV). Skipped because no audio-domain decoders
  are wired up server-side; a button that sends
  `SetDecoder(Ft8, true)` with no receiver would be cosmetic.
- **WSJT-X launcher** and **CONFIG dialog**. Both need
  side-channels (process spawn; settings pane) that don't fit
  this commit's scope.

### Added (server 0.9.0 — phase 4: REC feature goes live)
- **Disk recording of IQ or audio.** The phase-1
  `ClientMsg::StartRecording` / `ClientMsg::StopRecording` stubs are
  now real. Client triggers `StartRecording { kind: Iq | Audio,
  path: Option<String> }`; the server writes until `StopRecording`
  or clean shutdown. `ServerMsg::RecordingStatus` published every
  ~1 s while active (active / path / bytes / duration) so any
  connected client sees the recorder's progress.
- **File formats**: deliberately simple so a future in-pipeline
  replayer can consume them without a decoder:
  - **IQ** → raw `f32` interleaved `[I, Q]` pairs at the capture
    rate. Extension `.iq.f32`.
  - **Audio** → raw `f32` mono PCM at the audio output rate,
    captured *before* Opus encode (no server-side decoder needed).
    Extension `.pcm.f32`.
  Files are little-endian native. IQ samples are already normalised
  to `[-1, 1]` upstream.
- **Where files land**: `~/.local/state/efd-backend/recordings/` by
  default (new `[recording] directory` config key). Filenames
  default to `YYYYMMDD-HHMMSS-<kind>.<ext>`; clients can supply a
  path but it's sandboxed — absolute roots are stripped and
  `..` components dropped before joining to the recordings dir.
- **Pipeline changes**: new `pcm_tx: broadcast<Arc<Vec<f32>>>`
  channel inside `encode_audio_mux`, publishing post-DSP audio
  alongside the existing Opus-encode path. Recorder subscribes.
  Zero cost when nothing's recording (broadcast has no
  subscribers → `send` returns `Err` which the hot path ignores).
- **`postinst` pre-creates** the recordings directory with
  `efd:efd` ownership so first-boot recording works under the
  hardened systemd unit without manual setup.
- 5 new unit tests in `recording.rs` covering timestamp-breakdown,
  leap-year handling, path sandboxing, and default-filename logic.

### Deliberately deferred
- **WAV/Opus/FLAC output**. Raw f32 is the smallest viable format;
  users who want WAV for a media player can convert with
  `sox -r 48000 -e float -b 32 -c 1 <in>.pcm.f32 <out>.wav` or
  ffmpeg. A native-WAV recorder is a future commit.
- **Auto-rotation at size/time limits**. Recordings run until
  `StopRecording` or process shutdown.

### Fixed (server 0.8.4 — phase 3e systemd namespace regression)
- **`status=226/NAMESPACE` on upgrade to 0.8.3.** The phase 3e
  `ReadWritePaths=... /home/efd/.local/state/efd-backend` entry
  made systemd try to bind-mount a directory that the old (pre-
  phase-2) service user never had a chance to create, so namespace
  setup failed and the service refused to start. Two fixes:
  - `postinst` creates `~/.local/state/efd-backend` with the right
    ownership, so fresh installs + upgrades from any prior version
    land with the directory in place.
  - Unit prefixes the state path with `-` so a missing source
    directory is tolerated rather than failing the whole namespace
    (defensive for any box where postinst didn't run).

### Added (server 0.8.3 — phase 3e: process-respawn hot-swap + systemd fixes)
- **`SelectDevice` now actually switches.** Client-initiated
  `SelectDevice` writes the new device into the snapshot and
  triggers a clean process exit; systemd's `Restart=always`
  brings `efd-server` back ~2 seconds later with the new device
  active. The client's existing WS reconnect (with exponential
  backoff) covers the gap. This removes the "takes effect on next
  restart" caveat phase 2 shipped with.
- **`Pipeline.restart_requested_tx`** — new watch channel raised
  by the upstream `SelectDevice` handler. `main.rs` selects on it
  alongside SIGINT / SIGTERM in the graceful-shutdown future, so
  the restart path is a regular clean shutdown: HTTP server
  drains, pipeline cancels, snapshot saves to disk, process
  exits 0.
- **Systemd unit updates** (`server/debian/efd-server.service`):
  - `Restart=on-failure` → `Restart=always` so exit-0 on
    `SelectDevice` triggers the respawn.
  - `RestartSec=5` → `2` so the gap between exit and new process
    matches a user clicking a UI button.
  - `ReadWritePaths` now also covers `~/.local/state/efd-backend`.
    Phase 2's persistence writes to `$XDG_STATE_HOME` but the
    unit only whitelisted `~/.config`, so state saves were
    silently refused under systemd hardening. This is a bug fix;
    the `SelectDevice` respawn needs the snapshot to survive.

### Deliberately deferred (phase 3f)
- **True in-process hot-swap.** The mpsc channels the client-
  facing API exposes (`cat_tx`, `tx_audio_tx`) are coupled to the
  current source; a clean in-process swap needs a forwarder task
  pattern that decouples them. Planned for phase 3f.
- **Cross-kind device swap validation.** Today `SelectDevice`
  triggers a restart regardless of whether the target device has
  a driver. Unsupported kinds (`HackRf` / `RtlSdr` / `RspDx` /
  `IqFile`) restart into a server with no IQ source, which falls
  back to the SoftwareDemod-from-nothing path — not harmful, but
  not useful. Proper validation is phase 3f.

### Changed (server 0.8.2 — phase 3d: DRM single-USB IF feed)
- **DRM audio-IF is now a real USB-demod.** The demod's DRM branch
  used to `Re(filtered_buf)` with no post-filter shift, which
  folded the `-5 .. 0 kHz` and `0 .. +5 kHz` baseband halves onto
  the same `0..5 kHz` audio range — both sidebands of the OFDM
  block self-aliasing and degrading what DREAM received. Now a
  second NCO at +5 kHz (post-decimation, 48 kHz rate) shifts the
  filtered IQ block up to `(0 .. +10) kHz` before the real-part
  extraction, so the audio-IF stream DREAM consumes is a clean
  single-USB representation of the entire 10 kHz DRM block. Matches
  the "USB demodulation, LO at -5 kHz, 10 kHz BW" intent captured
  in `rework-architecture.md §3.3 / §8`. Offset lives as
  `DRM_USB_OFFSET_HZ`; if DREAM needs a different audio-IF placement
  (e.g. carrier at 12 kHz audio) the constant can move.

### Added (server 0.8.1 — phase 3b: NB + DNB impulse blankers go live)
- **Pre-IF `NB` does real work now.** `efd_dsp::nb::blank` is an
  envelope-threshold impulse blanker: EWMA-smoothed magnitude
  estimate, samples above `5×` the running mean get zeroed out,
  check-before-update so a single impulse doesn't poison the
  envelope for the next sample. Runs at 192 kHz on the IQ stream,
  ~5 ops/sample. Tunables (`ENV_ALPHA`, `BLANK_THRESHOLD`) are
  compile-time today; exposing a threshold slider is a future
  UI concern.
- **`DNB` does real work now.** `efd_dsp::audio_dsp::dnb` applies
  the same algorithm shape to the audio stream (mono f32 at 48 kHz
  post-demod / post-USB-capture / post-DRM) — absolute value, EWMA,
  zero on threshold. `AudioDsp` carries the envelope state across
  calls so `process` doesn't re-converge every frame.
- **`AudioDsp::process` is now `&mut self`.** Needed to mutate the
  stateful envelope tracker. Pipeline wires this through; the
  `encode_audio_mux` function holds a mutable `AudioDsp` instance
  and runs it on every outgoing audio block.
- 9 new unit tests covering pass-through, impulse blanking,
  sub-threshold survival, and envelope-bias behaviour.

### Not yet (phase 3c)
- **`DNR`** (spectral-subtraction denoise): needs an FFT + noise
  profile, likely wants a "learn noise floor now" button on the UI.
- **`DNF`** (adaptive notch): needs a notch-centre control; simple
  cases (single-tone heterodyne) can start with a fixed frequency.
- **`APF`** (audio peak filter): needs centre + width parameters.

### Changed (server 0.8.0, client 0.7.0 — **wire break**, phase 3a: pipeline topology)
- **Proto version 2 → 3.** Adds `ClientMsg::SetNb(bool)` (pre-IF
  noise blanker, the `NB` button in the UI) and
  `StateSnapshot.nb_on`. Server and client must be upgraded
  together; `WireError::VersionMismatch` flags skew cleanly.
- **Noise blanker moves pre-IF**, per the pipeline drawio. New
  `efd_dsp::nb` module with a `spawn_noise_blanker` task that
  sits between IQ capture and demod on its own broadcast channel
  (`iq_clean_tx`). The FFT task keeps subscribing to the raw
  `iq_tx` so the waterfall still shows the real spectrum; only
  the demod consumes the post-NB stream. Today's NB is a
  pass-through stub with an enable flag — the impulse-blanker
  math lands in a follow-up. The `NB` button in the UI and the
  `nb_on` snapshot field now have somewhere to go.
- **Audio-DSP flags wired end-to-end.** `AudioDsp` in
  `encode_audio_mux` now takes a `watch::Receiver<AudioDspFlags>`;
  a new pipeline task (`snapshot_dsp_propagator`) watches the
  session snapshot and pushes `nb_on` / `dnb_on` / `dnr_on` /
  `dnf_on` / `apf_on` out to the live pipeline. Stage
  implementations are still pass-through (phase 3b) but the
  toggle signal now reaches them and would take effect the
  instant a real filter lands.
- **Snapshot seeds the pipeline.** `initial_snapshot.nb_on` and
  `*_on` flags are honoured at startup so a user's persisted
  preferences (e.g. "I always want DNR on") come up before the
  first client connects.

### Fixed (server 0.7.2, client 0.6.1 — phase 2 noise cleanup)
- **Snapshot storm.** The snapshot tracker was calling
  `snapshot_tx.send_modify(...)` every CAT poll (~5 Hz) regardless
  of whether any tracked field had changed, which meant every
  connected client received a `ServerMsg::StateSnapshot` push on
  every tick. Switched to `send_if_modified` with per-field
  equality checks so notifications fire only on real change. The
  client's log spam (`state snapshot: freq=… mode=… device=None`)
  quiets to the actual edit rate.
- **Double "audio source changed" on connect.** Removed the client
  UI's unconditional `ClientMsg::SelectSource(Audio)` on init — a
  blind send that, when USB audio wasn't available, produced a
  noisy `RadioUsb → fallback to SoftwareDemod` pair in the server
  log. The server now keeps its own default; `SelectSource` only
  fires on explicit user click.

### Added (server 0.7.1 — phase 2 of rework, additive)
- **Device discovery at startup.** New `server/src/discovery.rs`
  walks `/sys/bus/usb/devices` for the known IQ USB VID/PIDs
  (FDM-DUO, HackRF, RTL-SDR RTL2832U, SDRplay RSPdx) and combines
  it with the existing `efd_audio::discover` + `/proc/asound/cards`
  sweep for audio-in devices. Results surface as the
  `efd_proto::DeviceList` pushed on WS connect and refreshed on
  client-initiated `EnumerateDevices`. Only FDM-DUO has a live
  driver in `efd-iq` today; other kinds are enumerated so the UI
  surfaces them honestly as "present but not yet supported".
- **Session-snapshot persistence.** New `server/src/persistence.rs`
  reads/writes an `efd_proto::StateSnapshot` as TOML at
  `$XDG_STATE_HOME/efd-backend/state.toml` (default
  `~/.local/state/efd-backend/state.toml`). Snapshot is loaded at
  startup, validated against the discovered device list (a gone
  device clears `active_device`), and used to seed the pipeline's
  session state. Saved at clean shutdown — freq / mode / BW follow
  the live `RadioState` automatically, so "where you were" is
  where you come back.
- **Live snapshot tracker.** New task inside `pipeline::start`
  subscribes to `state_tx` and keeps `snapshot_tx.freq_hz /
  mode / filter_bw_hz / rit_hz / xit_hz / if_offset_hz` in sync
  with the radio, so shutdown persistence captures real values.
- **Phase-1 stubs are now real**. `EnumerateDevices` re-runs
  discovery and pushes the updated list.  `SelectDevice`
  updates `snapshot.active_device` + `device_list.active`.
  `SetDecoder` / `SetDnb` / `SetDnr` / `SetDnf` / `SetApf`
  update the snapshot so their intent survives a restart.
  `SaveState` writes the current snapshot to disk. `LoadState`
  reads + validates the on-disk state back into the live
  `snapshot_tx`.
- **Connect-time hydration.** Downstream pushes `Capabilities`,
  `DeviceList`, and `StateSnapshot` before the first `FftBins` /
  `RadioState` so clients can pre-fill their UI without waiting
  for the first poll.
- **Downstream watches.** `device_list_tx` and `snapshot_tx` are
  now watch channels the downstream task subscribes to, so any
  client-initiated mutation propagates to every connected session.

### Not yet (phase 3+)
- **Hot-swap** is deferred. A `SelectDevice` call today persists
  the choice to the snapshot and updates the client-visible
  `active` marker, but the pipeline itself keeps running the
  device it was started with. To switch devices, restart the
  server (systemd `Restart=always` will pick up the new snapshot
  automatically). Phase 3's pipeline-topology rewrite is the
  natural home for in-process swap.
- **SAM / DSB demods** still fall through to envelope AM.
- **DSP block** (DNB/DNR/DNF/APF) is still a pass-through stub.

### Changed (server 0.7.0, client 0.6.0 — **wire break**, phase 1 of rework)
- **Proto version 1 → 2.** Old server/client pairs cannot talk to the
  new ones; the existing `WireError::VersionMismatch` handshake
  detects skew cleanly. Server and client must be upgraded together.
- **`Mode` gains `SAM`, `SAMU`, `SAML`, `DSB`.** The software demod
  doesn't implement SAM / DSB yet (phase 3) — current code falls
  through to envelope-AM for these variants so the pipeline keeps
  running. Hardware CAT maps all four to AM digit 5, matching the
  existing DRM convention.
- **`DecoderKind` enum added** (Cw/Rtty/Psk/Mfsk/Fax/Pckt/Wspr/
  Ft8/Aprs) and surfaced on `Capabilities::supported_decoders` and
  `ServerMsg::DecodedText`.
- **Device model scaffolding**: new `SourceClass` (`Audio` / `Iq`),
  `DeviceId { kind, id }`, `RecKind`. Added client → server messages
  `EnumerateDevices`, `SelectSource`, `SelectDevice`, `SetDecoder`,
  `SetDnb`/`SetDnr`/`SetDnf`/`SetApf`, `StartRecording`/
  `StopRecording`, `SaveState`/`LoadState`. Server stubs them
  (debug-logs and ignores) until phases 2–4 land.
- **Server messages added**: `DeviceList`, `DecodedText`,
  `RecordingStatus`, `StateSnapshot`. Client stubs them (eprintln)
  until the UI rewrite (phase 5).
- **`RadioState` extended** with `filter_bw_hz: Option<f64>`,
  `rit_hz`/`rit_on`, `xit_hz`/`xit_on`, `if_offset_hz`, and
  `snr_db: Option<f32>`. Server fills them with safe defaults
  today (None / 0 / false); real RIT/XIT/IF surfacing is phase 3
  work, and SNR comes from the demod in phase 3.
- **`GridCell` enum added** in `efd-proto/grid.rs` — stable IDs for
  the client's named layout cells. Unused on the wire today;
  available for phase 5.
- **`ClientMsg::SetAudioSource` removed**, replaced with
  `SelectSource(SourceClass)`. Server's internal `AudioRouting` enum
  (in `pipeline::AudioRouting`) keeps the RadioUsb / SoftwareDemod
  routing semantics intact — `From<SourceClass>` bridges the new
  message onto the old pipeline until phase 2.

### Changed (server 0.6.14)
- **s16↔f32 scaling is now symmetric on the decode path.** The USB-RX
  audio tap (`efd-audio/usb_rx`) and the DRM null-sink reader
  (`efd-dsp/drm`) previously divided s16 samples by 32768 while their
  encoder counterparts multiplied by 32767, leaving a ~0.003 dB
  asymmetry and a slight level drift across a DRM round-trip. Both
  decode sites now use 32767, matching `f32_to_s16` exactly.
- **CAT mutex poison is logged once.** `lock_port` in `efd-cat/poll`
  used to emit a `warn!` on *every* acquisition after a poison event —
  the poll task's 200 ms cadence would flood the journal. An
  `AtomicBool` now gates a single `error!` on the healthy→poisoned
  transition; the original panic is what the operator should read.

### Added (server 0.6.13)
- **`efd-server --version` / `-V`.** Prints `efd-server <version>` and
  exits, without initialising tracing, loading config, or entering the
  tokio runtime. The startup log also now carries `version=<x>` so a
  running binary's version is visible without asking `dpkg`.

### Changed (server 0.6.12)
- **rigctld `F` (set-frequency) sanity bounds.** The responder now
  rejects frequencies outside `[1 kHz, 99_999_999_999 Hz]` with
  `RPRT -11` (invalid parameter) before the value hits the native
  CAT command builder. Native CAT is `FA<11-digit-hz>;`, so values
  ≥ 100 GHz would have produced a malformed frame; the lower bound
  catches 0 and absurd typos. This is a wire-sanity check, not a
  per-device capability check — device-specific rejection still comes
  from the radio's firmware.
- **WS `validate_cat_command` returns a reason.** Rejected client CAT
  commands now log *why* (length, missing terminator, non-uppercase
  prefix, prefix not on allowlist, or non-printable payload) alongside
  the offending string. Diagnostic-only; the set of accepted commands
  is unchanged.
- Skipped **slow-consumer lag disconnect** (originally on the Batch D
  list): the existing `SEND_TIMEOUT = 2s` on `sink.send` already
  closes any client that actually wedges a broadcast, and broadcast
  `Lagged` on a healthy-but-slow client is self-healing. Adding a
  separate lag-count threshold would be speculative and risked
  spurious disconnects on transient network stalls.

### Changed (client 0.5.3)
- **Audio ring buffer switches to drop-newest.** `AudioPlayer::push_audio`
  previously drained the oldest samples when the ring overflowed,
  producing a mid-stream click in whatever SSB/AM audio was currently
  playing. Now an overflow truncates the tail of the incoming Opus
  frame instead: what's already in the ring plays out cleanly, and a
  sustained producer-outruns-consumer state surfaces as a gap at a
  20 ms frame boundary. Rate-limited stderr warn at power-of-two drop
  counts. Bounded latency stays at `RING_CAPACITY` (1.5 s) either way.
- **Client mutex locks tolerate poisoning.** Two bare `.unwrap()` calls
  on `fft_data` and `radio_state` mutexes in the GTK tick callback
  replaced with `unwrap_or_else(|e| e.into_inner())`, matching the
  existing pattern used for the WS message queue. Prevents the GTK
  main loop from crashing if the WS thread ever panics while holding
  a lock.

### Changed
- **Bounded CAT serial I/O.** `SerialPort::command` wraps write(2) and
  `read_response` in a 500 ms overall deadline via `poll(2)` (new `nix`
  `poll` feature). A mid-session USB unplug — where the kernel is
  still fielding write(2) or the radio has gone silent — now surfaces
  as a `TimedOut` error and the CAT poll/command task recovers
  instead of wedging the pipeline indefinitely.
- **Demod uses `try_send` with drop-on-full backpressure.** Previously
  `audio_tx.blocking_send` would stall the demod if the downstream
  audio consumer (ALSA or WS encoder) fell behind; a USB-audio
  underrun could pause IQ consumption, NCO state, and the whole FFT
  pipeline. Now the demod drops the current audio block on a full
  queue, warns at exponentially spaced counts (1, 2, 4, 8, …), and
  keeps the RX chain running. Short audio glitch beats a frozen
  waterfall.

### Added
- **DRM bridge pre-flight validation.** `run_bridge` now rejects
  invalid PipeWire sink names (`[a-z0-9_]`, 1..=63 chars) and a
  missing / non-regular `DrmInput::File` path before spawning any
  subprocess. Operators see a clear `DspError::Drm` instead of a
  downstream `pactl` or DREAM failure. Sink-name validation also closes
  the `pactl load-module` argument-boundary confusion surface (e.g.
  `sink_name=foo bar`).
- **Noisier IF-parser + IQ convert.** `parse_if_response` uses
  `str::get(..)` so a short frame or non-ASCII byte returns `None`
  instead of panicking at a UTF-8 char boundary. `fdm-duo`
  `convert_samples` logs a `warn!` when the USB buffer isn't a
  multiple of 8 bytes (torn packet); prior code silently dropped the
  tail.
- **Optional WebSocket auth via shared token.** New
  `[server] auth_token` in `config.toml`; when set, clients must pass
  `?token=<value>` on the WS URL or the upgrade is rejected with 401
  (constant-time compare to avoid timing leaks). When unset the server
  is unauthenticated as before — intended for loopback or trusted
  LAN. Startup emits a loud warning if `bind` is non-loopback and no
  token is configured. The rigctld responders (fdmduo-front,
  demod-front) also warn when bound to a non-loopback address, since
  they have no auth layer — recommended pattern is to keep them on
  `127.0.0.1` and SSH-forward from the client
  (`ssh -L 4532:localhost:4532 pi@cm5`).

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
