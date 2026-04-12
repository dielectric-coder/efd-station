# efd-station — CLAUDE.md

Multi-SDR station application centered on the Elad FDM-DUO but extensible to
RSPdx, HackRF, RTL-SDR, and a plain analog portable radio — CM5 backend +
GTK4 native client, single Cargo workspace.
Repo: github.com/dielectric-coder/efd-station
This file is the authoritative project context. Update it as decisions are made.

Architecture diagram: `docs/CM5-sdr-backend.drawio` (+ `.svg` export).

---

## Hardware context

- **Host**: Raspberry Pi CM5 on a Waveshare CM5-PoE-Base-A IO board (headless, no GUI).
- **Audio output**: Pi HAT sound card → amplifier (ALSA, local playback) **or** USB audio dongle. Exactly one is selected per config.
- **Network**: Ethernet/WiFi — serves WebSocket clients on LAN or internet.
- **GPIO front panel**: minimal, future use — reset button, lock, status indicators (connected-to-hardware, running). Not used for tuning or CAT control.
- **Supported RF sources** (one active at a time, selected by config):

| Source | IQ path | Audio path | Hardware CAT | Software demod | TX |
|---|---|---|---|---|---|
| `FDM-DUO` (MON) | — | FDM-DUO USB audio passthrough | direct USB serial (full native CAT, owned by `efd-cat`) | no | yes |
| `FDM-DUO` (SDR) | FDM-DUO USB IQ (vendor-native) | from software demod | direct USB serial (full native CAT, owned by `efd-cat`) | yes | yes |
| `HackRF` | libhackrf | from software demod | — | yes | yes |
| `RSPdx` | SDRplay API | from software demod | — | yes | no |
| `RTL dongle` | librtlsdr | from software demod | — | yes | no |
| `portable radio` | — | USB audio dongle (analog in) | — | no (audio-domain decoders only) | no |

TX capability is per-device and advertised to the client via capability flags.

---

## Operating modes

Two orthogonal axes:

### Backend source mode (how audio is produced)
- **MON** — FDM-DUO is used as a conventional receiver. Audio comes from the radio's USB audio port. CAT reports the radio's current state (freq, mode, BW, RIT, XIT, etc.); the backend only exposes/augments CAT, it does not demodulate.
- **SDR** — Any supported source feeds IQ into the software demodulator. Audio comes from the demod. The demod presents *itself* as a radio via a rigctld-compatible CAT endpoint.

FDM-DUO can run in either mode (including both at once, since it provides IQ *and* audio *and* CAT independently over USB). Other SDR hardware can only run SDR mode. The portable-radio config is a third path: audio-only, no IQ, no CAT — but audio-domain DSP decoders (WEFAX, RTTY, CW, PSK, etc.) still run over the incoming audio.

Mode selection and runtime state machine lives in the server (`pipeline.rs`). See commit history around the MON/SDR state machine work.

### Client consumption mode (who's listening)
- **Standalone** — CM5 + HAT/dongle + amp only. No network client. Usable as a tabletop radio.
- **Remote** — Any machine on the network runs the GTK4 client. Receives FFT bins, audio, and radio state over WebSocket; sends CAT, TX audio, and PTT back.

Both consumption modes can run simultaneously.

---

## Workspace layout

```
<repo-root>/                    # single repo — server + client + shared crates
├── CLAUDE.md
├── Cargo.toml                  # workspace
├── docs/                       # architecture diagrams (drawio + svg)
├── server/                     # [[bin]] CM5 backend (Axum, tokio pipeline)
│   └── src/
│       ├── main.rs             # startup, config (~/.config/efd-backend/config.toml)
│       ├── pipeline.rs         # spawns all tasks, wires broadcast/mpsc channels,
│       │                       # owns MON/SDR mode state machine
│       └── ws/
│           ├── downstream.rs   # server → client: FFT bins, audio, radio state
│           └── upstream.rs     # client → server: PTT, CAT commands, TX audio
├── client/                     # [[bin]] GTK4 native client
│   └── src/
│       ├── main.rs
│       ├── ui/                 # GTK4 widgets (dashboard, spectrum, waterfall, controls)
│       └── ws.rs               # WebSocket connection to server
├── third_party/
│   └── dream/                  # vendored DREAM 2.1.1 + hamlib cast patch + build script
└── crates/
    ├── efd-iq/                 # Multi-backend IQ capture — trait + per-device drivers (feature-gated)
    │   └── src/
    │       ├── lib.rs          # re-exports, open_device() factory
    │       ├── source.rs       # trait IqSource + Capabilities + errors
    │       ├── types.rs        # IqBlock, SampleFormat, TuneRequest, Gain, StreamConfig
    │       └── drivers/
    │           ├── fdm_duo.rs  # feature="fdm-duo"  vendor-native (EladSpectrum port)
    │           ├── hackrf.rs   # feature="hackrf"   libhackrf
    │           ├── rspdx.rs    # feature="rspdx"    SDRplay API
    │           └── rtl.rs      # feature="rtl"      librtlsdr
    ├── efd-dsp/                # Three tiers: FFT (orthogonal), analog demod, IQ codecs, audio decoders
    │   └── src/
    │       ├── fft/            # orthogonal — spectrum producer (IQ or audio source)
    │       ├── demod/          # Tier 1: analog IQ demod — ONE task, mode param (AM/SAM/USB/LSB/CW±/NFM/WFM)
    │       ├── codec/          # Tier 2: IQ-domain codecs — drm.rs (DREAM bridge), freedv.rs
    │       └── decoder/        # Tier 3: audio-domain decoders — cw/rtty/psk/wspr/ft8/aprs/wefax
    ├── efd-audio/              # ALSA HAT / USB dongle output + USB audio TX
    ├── efd-cat/                # direct USB serial CAT (FDM-DUO) + rigctld-compatible responder for external apps
    └── efd-proto/              # shared WS message types — used by server AND client
```

`efd-proto` is the contract between server and client. A breaking change to any
message type fails to compile both binaries simultaneously.

---

## Crate responsibilities

### efd-iq
- **Driver-per-device architecture**. A single `IqSource` trait dispatches to the active device; each backend lives in its own module under `drivers/` behind a cargo feature flag. Adding a new SDR = one new file + one feature + one factory arm.
- Trait surface (stable contract for all drivers):
  ```rust
  #[async_trait]
  pub trait IqSource: Send {
      fn capabilities(&self) -> &Capabilities;       // freq range, sample rates, gain model, TX?
      async fn start(&mut self, cfg: StreamConfig)
          -> Result<broadcast::Receiver<IqBlock>>;
      async fn stop(&mut self) -> Result<()>;
      async fn tune(&mut self, req: TuneRequest) -> Result<()>;
      async fn set_sample_rate(&mut self, sr: u32) -> Result<()>;
      async fn set_gain(&mut self, g: Gain) -> Result<()>;
  }
  ```
- Drivers (one file each, feature-gated — a CM5 without SDRplay headers simply disables `rspdx`, no stubs needed):
  - **`drivers/fdm_duo.rs`** — USB IQ via vendor-native code (port of EladSpectrum)
  - **`drivers/hackrf.rs`** — libhackrf
  - **`drivers/rspdx.rs`** — SDRplay API (proprietary)
  - **`drivers/rtl.rs`** — librtlsdr
- `lib.rs::open_device(&DeviceConfig) -> Result<Box<dyn IqSource>>` resolves the active driver from config at startup.
- Per-driver `Capabilities` flows up into `efd-proto::Capabilities` so the client greys out controls the device can't do (RSPdx: no TX; HackRF: no hardware CAT; etc.).
- SoapySDR is deliberately *not* used for now (no reliable FDM-DUO support). The trait is its own small abstraction; SoapySDR could be added later as a fifth driver if the matrix grows.
- Produces `IqBlock` items on a `tokio::sync::broadcast` channel. No DSP here — raw IQ only.
- MON-only and portable-radio configs skip `open_device()` entirely; the pipeline's IQ fan-out branch stays uninstantiated.

### efd-dsp
Organized as **three tiers** plus an orthogonal FFT producer. Tiers 1 and 2 are mutually exclusive at runtime (one produces `AudioSamples` at a time, selected by current `Mode`); Tier 3 is always-on and mode-agnostic.

**FFT (`fft/`) — orthogonal spectrum producer**
- FFTW3 bindings + volk for SIMD acceleration; applies window, computes magnitude bins, publishes `FftBins` to WS downstream.
- `fft/iq.rs` runs off `broadcast<IqBlock>` in SDR mode. `fft/audio.rs` runs off `broadcast<AudioSamples>` in MON/portable mode (audio spectrum). Runs continuously regardless of which tier is producing audio.

**Tier 1 — Analog IQ demod (`demod/`)**
- ONE task, ONE `broadcast<IqBlock>` subscription, mode parameter: `AM / SAM / USB / LSB / CWU / CWL / NFM / WFM`.
- Shared product-detector / envelope / SAM-PLL / FM-discriminator paths; differences are filter shapes (`demod/filter.rs`) and BFO offsets (`demod/bfo.rs`).
- Intra-tier mode switches (USB → AM → CW…) reconfigure the existing task — no teardown, no channel re-plumbing. Cheap.
- Publishes to the shared `broadcast<AudioSamples>` consumed by ALSA, WS, and Tier 3.

**Tier 2 — IQ-domain codecs (`codec/`)**
Each is its own subsystem with a full spawn/teardown lifecycle. Switching *into* or *out of* Tier 2 (e.g. USB → DRM, DRM → FreeDV) fully tears down the previous producer.
- `codec/drm.rs` — bridges `broadcast<IqBlock>` to the DREAM subprocess via PipeWire null sinks:
  - Decimates IQ to 48 kHz, packs as interleaved s16 L=I / R=Q, writes to `drm_in` sink
  - Spawns DREAM: `-I drm_in.monitor -O drm_out -c 6 --sigsrate 48000`
  - Reads decoded stereo PCM from `drm_out.monitor`, publishes to the shared `broadcast<AudioSamples>`
  - Active only when current `Mode` is `DRM`; torn down on mode change
- `codec/freedv.rs` — FreeDV codec, same output fan-out. Implementation detail (in-process vs. DREAM-style subprocess bridge) is left open; the PipeWire null-sink pattern from `drm.rs` is available as a template if needed.

**Tier 3 — Audio-domain decoders (`decoder/`)**
- Consume `broadcast<AudioSamples>` regardless of origin (Tier 1, Tier 2, *or* the MON/portable audio-in path from `efd-audio`).
- N decoders can run in parallel — enabled by config (`decoder.enabled = ["cw", "ft8"]`), not tied to the current radio `Mode`.
- `decoder/` members: `cw.rs`, `rtty.rs`, `psk.rs`, `wspr.rs`, `ft8.rs`, `aprs.rs` (AFSK 1200), `wefax.rs`. Each emits `DecodedText` (or a typed variant) to WS downstream.
- Advertised to the client via a separate `supported_audio_decoders` capability field — works in MON and portable configs just as well as SDR.

**State-machine placement**
- `server/pipeline.rs` owns the `Mode` state machine. On mode change it picks which Tier-1/Tier-2 producer to run and handles teardown/spawn.
- `efd-dsp` exposes three thin spawners: `demod::spawn_analog`, `codec::drm::spawn`, `codec::freedv::spawn`. Tier-1's task also exposes `set_mode()` for intra-tier changes that don't require respawn.

### efd-audio
- **ALSA output task**: consumes `broadcast<AudioSamples>`, writes to HAT sound card *or* USB audio dongle (whichever is configured).
  - Provides standalone audio output to amp, no network needed.
- **Audio input task** (MON + portable-radio configs): reads from FDM-DUO USB audio or USB audio dongle analog-in, publishes to `broadcast<AudioSamples>`. This is what lets audio-domain decoders run without an IQ source.
- **USB audio TX task**: consumes `mpsc<TxAudio>` from WS upstream, writes to FDM-DUO USB audio (FDM-DUO TX) or HackRF TX path. Only active when the configured device supports TX.
  - TX audio originates from client mic, WSJT-X, FLDIGI, FreeDV, etc.
  - Client machines use PipeWire virtual audio device to feed digital mode apps.

### efd-cat
`efd-cat` owns all CAT surfaces. hamlib `rigctld` is **not** used on the CM5 — we bypass it for full native-CAT command coverage on the FDM-DUO. Responsibilities:

1. **Direct USB serial CAT to FDM-DUO** (`serial.rs`, `parse.rs`, `poll.rs`) — active in FDM-DUO configs (MON or SDR). We speak the radio's full native CAT dialect, not a subset. Publishes `broadcast<RadioState>` and accepts internal `mpsc<CatCommand>` from WS upstream.
2. **Hand-rolled rigctld-compatible TCP responder** — our own implementation of the hamlib rigctld protocol subset that external apps (WSJT-X, FLDIGI, digital-mode tooling) actually use. Grown on demand. Two instances with different backing targets can bind simultaneously:
   - **Port A — FDM-DUO front**: translates incoming rigctld commands into native FDM-DUO CAT over our serial link. Lets WSJT-X etc. reach the radio without a second process fighting for the USB serial port. Active in FDM-DUO configs.
   - **Port B — demod front**: exposes the software demod as if it were a radio. Active in any SDR-mode config.
3. Publishes merged `broadcast<RadioState>` consumed by WS downstream. In FDM-DUO SDR mode, state from both surfaces (radio + demod) is unified; the client sees one logical radio with capability flags indicating what is hardware vs software.

Endpoint layout:

| Config | USB serial to FDM-DUO | Responder port A (FDM-DUO front) | Responder port B (demod front) |
|---|---|---|---|
| FDM-DUO MON | yes | yes | — |
| FDM-DUO SDR | yes | yes | yes |
| HackRF / RSPdx / RTL | — | — | yes |
| portable radio | — | — | — |

Exact port numbers and defaults TBD in config.

### efd-proto
- Shared serde types for all WebSocket messages.
- **Server → client (downstream)**:
  - `FftBins` — magnitude bin array + metadata (center freq, span, ref level)
  - `AudioChunk` — encoded audio (Opus wideband 48 kHz)
  - `RadioState` — frequency, mode, BW, ATT, LP, AGC, NR, NB, S-meter, RX/TX
  - `Capabilities` — per-source: has_iq, has_tx, has_hardware_cat, supported_demod_modes, etc.
  - `DecodedText` — output from audio-domain decoders (WEFAX/RTTY/CW/PSK)
  - `Error`
- **Client → server (upstream)**:
  - `CatCommand` — frequency set, mode, BW, ATT, LP, AGC, NR, NB, RIT, XIT, etc.
  - `TxAudio` — encoded TX audio chunk
  - `Ptt` — PTT on/off

### server (binary)
- Axum HTTP + WebSocket server.
- `pipeline.rs` spawns all tasks and wires channels:
  - `broadcast` channels for fan-out: IqBlock, AudioSamples, RadioState, FftBins
  - `mpsc` channels for single consumers: TxAudio → efd-audio, CatCommand → efd-cat
- Owns the MON/SDR mode state machine; parameter persistence across mode switches.
- WebSocket handler manages per-client state and routes messages.
- Advertises capabilities to each client on connect.

---

## Tokio pipeline (RX path, SDR mode)

```
[IQ capture / efd-iq drivers/*]   (exactly one driver active — fdm_duo/hackrf/rspdx/rtl)
        │
        └─ broadcast<IqBlock>
               ├──→ [fft::iq / efd-dsp]              → FftBins → [WS downstream] → clients
               │
               ├──→ [demod (Tier 1) / efd-dsp]       → broadcast<AudioSamples>
               │        AM/SAM/USB/LSB/CW±/NFM/WFM — one task, mode param
               │
               └──→ [codec (Tier 2) / efd-dsp]       → broadcast<AudioSamples>
                        codec::drm ─→ PipeWire null-sink(drm_in)
                                       │
                                  dream -I drm_in.monitor -O drm_out -c 6 --sigsrate 48000
                                       │
                                  PipeWire null-sink(drm_out).monitor ─→ shared AudioSamples
                        codec::freedv ─→ (in-process or subprocess bridge — TBD)

  (Tiers 1 and 2 are mutually exclusive — exactly one producing AudioSamples at a time)

  broadcast<AudioSamples>
         ├──→ [ALSA / efd-audio] → HAT/dongle → amp
         ├──→ [decoder (Tier 3) / efd-dsp]  → DecodedText → [WS downstream]
         │        cw / rtty / psk / wspr / ft8 / aprs / wefax — N in parallel, config-driven
         └──→ [WS downstream] → clients
```

## Tokio pipeline (RX path, MON + portable-radio modes)

```
[audio in / efd-audio]   (FDM-DUO USB audio or USB dongle analog-in)
        │
        └─ broadcast<AudioSamples>
               ├──→ [ALSA / efd-audio] → HAT/dongle → amp
               ├──→ [decoder (Tier 3) / efd-dsp] → DecodedText → [WS downstream]
               ├──→ [fft::audio / efd-dsp] → FftBins (audio spectrum) → [WS downstream]
               └──→ [WS downstream]    → clients

  (Tiers 1 and 2 are not instantiated — no IQ source in MON / portable configs)
```

## Tokio pipeline (TX + CAT path)

```
clients → [WS upstream] ──→ mpsc<TxAudio>    → [USB audio TX / efd-audio] → FDM-DUO or HackRF
                        └──→ mpsc<CatCommand> → [efd-cat]
                                                     ├─↔ USB serial (FDM-DUO native CAT)
                                                     ├─↕ TCP responder port A (FDM-DUO front, for WSJT-X etc.)
                                                     └─↕ TCP responder port B (demod front, for WSJT-X etc.)
                                                                │
                                                     broadcast<RadioState> (merged radio + demod)
                                                                │
                                                     [WS downstream] → clients
```

Channel type summary:
- `tokio::sync::broadcast` — IqBlock, AudioSamples, FftBins, RadioState, DecodedText (fan-out)
- `tokio::sync::mpsc` — TxAudio, CatCommand (single consumer)

---

## External dependencies (runtime, CM5)

- **hamlib rigctld**: *not* used on the CM5. We own the FDM-DUO USB serial port directly inside `efd-cat` so we can speak full native CAT instead of hamlib's subset. External apps (WSJT-X etc.) reach the radio through our own rigctld-compatible responder instead.
- **Our rigctld-compatible responder** (inside `efd-cat`, not an external dep): one instance per bound port. Port A fronts the FDM-DUO (translating rigctld to native CAT); port B fronts the software demod in SDR mode. WSJT-X and other hamlib apps connect to these the same way they would connect to hamlib's rigctld.
- **Vendor SDR libraries**: `libhackrf`, `SDRplay API`, `librtlsdr` — linked in per the active device.
- **ALSA**: HAT and/or USB-dongle audio output.
- **PipeWire**: present on the CM5 (Trixie ships it) — used for the two virtual null sinks that bridge the `efd-dsp` DRM task to the DREAM subprocess. Also required on *client* machines for the virtual audio device that feeds digital-mode apps.
- **DREAM (2.1.1 console build)**: DRM decoder subprocess. Vendored under `third_party/dream/` because the in-distro `dream-drm` (2.2) has known decoding regressions and the build needs `qmake CONFIG+=console` plus a one-line `rig_model_t` cast patch to compile against modern hamlib. The subprocess reads IQ from one PipeWire null sink and writes decoded audio to another; the Rust pipeline feeds/consumes the monitors.
- **FreeDV**: codec, runs on CM5. Same audio path as DREAM (virtual sink bridge pattern is the template).

---

## Key decisions (do not re-litigate without good reason)

| Topic | Decision |
|---|---|
| IQ over network | No. CM5 runs FFT, sends only magnitude bins to clients. |
| Audio codec (network) | Opus wideband, 48 kHz (not VoIP 8 kHz profile) |
| IQ backend abstraction | `IqSource` trait in `efd-iq` with one driver per device under `drivers/*`, each behind a cargo feature flag. Vendor-native (no SoapySDR for now); adding a new SDR = one new file + one feature + one factory arm. |
| DSP organization | Three tiers in `efd-dsp`: (1) analog IQ demod — one task, mode param (AM/SAM/USB/LSB/CW±/NFM/WFM); (2) IQ-domain codecs — DRM, FreeDV, each its own subsystem; (3) audio-domain decoders — RTTY/PSK/WSPR/FT8/APRS/CW/WEFAX, N in parallel off `AudioSamples`. Tiers 1 & 2 mutually exclusive; Tier 3 always-on and mode-agnostic. FFT is orthogonal to all three. |
| Hardware CAT | `efd-cat` owns FDM-DUO USB serial directly. hamlib `rigctld` is not used — we speak full native CAT instead of hamlib's subset (FDM-DUO is the only source with hardware CAT, so coverage matters more than ecosystem compat at the wire level). |
| External-app CAT access | Hand-rolled rigctld-compatible responder in `efd-cat`, one codebase serving two bound ports with different backends: port A translates rigctld → native CAT for the FDM-DUO; port B fronts the software demod. Grown on demand as WSJT-X/FLDIGI/etc. require commands. |
| Two-responder model | When FDM-DUO is in SDR mode, both ports bind. Client sees one logical radio via merged RadioState + capability flags indicating hardware vs software. |
| DSP location | All DSP (FFT, demod, DRM, FreeDV, audio decoders) on CM5. Clients receive processed data only. |
| Audio-domain decoders | WEFAX/RTTY/CW/PSK operate on `AudioSamples` regardless of origin (IQ demod, FDM-DUO audio passthrough, or analog audio via dongle). |
| TX scope | RX-first. TX supported only on FDM-DUO and HackRF. Other configs advertise `has_tx=false`. |
| TX audio source | From client over WebSocket (mic, WSJT-X, FLDIGI, FreeDV via PipeWire). |
| Audio fan-out | In-process tokio broadcast channels for everything except DRM. PipeWire null sinks bridge IQ↔DREAM and DREAM audio↔pipeline; everything else stays in-process. |
| DRM decoder | Vendored DREAM 2.1.1 (`third_party/dream/`) built console-only with a hamlib cast patch. Spawned as a subprocess on DRM mode selection; IQ fed via PipeWire null sink (`drm_in`) at 48 kHz stereo L=I R=Q (`-c 6`, I/Q zero IF); decoded audio read from the monitor of a second null sink (`drm_out`). DREAM stays unpatched beyond the hamlib fix. |
| DRM-in-MON | Not supported — the radio's built-in AM demod destroys the OFDM signal. DRM requires SDR mode so the raw IQ reaches DREAM. |
| Audio output hardware | HAT *or* USB dongle — exactly one per config. |
| GPIO scope | Status indicators + reset/lock only. No tuning/CAT control via GPIO. Future use. |
| Language / framework | Rust, Axum, tokio async runtime. |
| FFT library | FFTW3 + volk (from EladSpectrum). |
| WS serialization | bincode — both ends Rust, lowest overhead for FftBins at video rate. If a browser client ever needed, revisit MessagePack. |
| Client window host | GTK4 + GtkGLArea for OpenGL spectrum/waterfall. gtk4-rs bindings. |
| Config format | TOML. |
| Config location | `~/.config/efd-backend/config.toml`. |

---

## UI (client-side)

The GTK4 client lives in `client/`. Relevant context:
- Spectrum/waterfall rendering: glSpectrum via GtkGLArea (OpenGL)
- Window host: GTK4 (gtk4-rs)
- Receives FftBins → renders spectrum + waterfall (IQ spectrum in SDR mode, audio spectrum in MON/portable)
- Receives AudioChunk → decodes + plays back
- Receives RadioState + Capabilities → enables/disables controls based on what the active source supports
- Receives DecodedText → renders in a decoder panel

UI wireframe controls (for reference when designing RadioState and CatCommand types):
- VFO A/B toggle, SPLIT, frequency (Hz), step, mode, BW
- S-meter (RX) / Power meter (TX), SNR (RX only)
- RIT on/off + delta (Hz), XIT on/off
- Memory store/recall
- ATT (on/off toggle), LP 50 MHz (on/off toggle)
- NR (popup), NB (popup), AGC (popup), AGC threshold, auto-notch (on/off)
- CW WPM, CW tone (Hz)
- PTT (toggle, spacebar) — hidden/disabled when `has_tx=false`
- Ref level (dBm), span (kHz), WF speed, WF palette
- Spectrum panel + waterfall panel (freq axis + filter passband overlay)

UI design itself comes after the backend architecture settles.

---

## Pending decisions

- [ ] FftBins precision: f32 vs f16 (display needs ~11 bits of mantissa; halves bin payload ~4 KB → ~2 KB at 1024 bins/frame)
- [ ] FftBins encoding: raw array vs delta/compressed
- [ ] Audio chunk size / latency budget for remote operation
- [ ] TLS / authentication for remote WebSocket clients
- [ ] PTT sequencing: who arbitrates PTT when a digital mode app (WSJT-X) and the client UI both have PTT capability?
- [ ] Default TCP port numbers for the two rigctld-compatible responders (FDM-DUO front and demod front)
- [ ] Exact subset of rigctld protocol commands our responder must support on day one (driven by WSJT-X/FLDIGI needs first)
- [ ] Mapping table: rigctld command subset ↔ FDM-DUO native CAT commands (for responder port A)
- [ ] Capability-flag schema in `efd-proto::Capabilities`

---

## Repo

github.com/dielectric-coder/efd-station
Related: github.com/dielectric-coder/EladSpectrum (FDM-DUO IQ code to reuse in `efd-iq`)
