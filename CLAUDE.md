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
└── crates/
    ├── efd-iq/                 # Multi-backend IQ capture (vendor-native APIs)
    ├── efd-dsp/                # FFT (FFTW3 + volk), demod, audio-domain decoders
    ├── efd-audio/              # ALSA HAT / USB dongle output + USB audio TX
    ├── efd-cat/                # direct USB serial CAT (FDM-DUO) + rigctld-compatible responder for external apps
    └── efd-proto/              # shared WS message types — used by server AND client
```

`efd-proto` is the contract between server and client. A breaking change to any
message type fails to compile both binaries simultaneously.

---

## Crate responsibilities

### efd-iq
- Multi-backend IQ capture abstraction. One enum/trait dispatches to the active device:
  - **FDM-DUO**: USB IQ via vendor-native code (from EladSpectrum)
  - **HackRF**: libhackrf
  - **RSPdx**: SDRplay API
  - **RTL dongle**: librtlsdr
- SoapySDR is deliberately *not* used for now (no reliable FDM-DUO support). Revisit later as a unifying layer.
- Produces `IqBlock` items on a `tokio::sync::broadcast` channel.
- No DSP here — raw IQ only.
- Inactive in MON-only and portable-radio configs.

### efd-dsp
- **FFT task**: consumes `broadcast<IqBlock>`, produces magnitude bins
  - FFTW3 bindings + volk for SIMD-accelerated processing
  - Applies window function, computes magnitude spectrum
  - Publishes `FftBins` to WS downstream
- **IQ demod task**: consumes `broadcast<IqBlock>`, produces `AudioSamples`
  - Modes: SSB, AM, FM, DRM (via DREAM), FreeDV
  - DREAM and FreeDV run on CM5 — decoded audio feeds the same audio path
  - Publishes `broadcast<AudioSamples>` for fan-out to ALSA and WS
- **Audio-domain decoders**: consume `broadcast<AudioSamples>` (from the IQ demod *or* from incoming audio in MON/portable configs), produce decoded content (WEFAX, RTTY, CW, PSK, etc.). Runs identically regardless of audio origin.
- In MON mode the FFT task can optionally operate on the audio stream (audio spectrum) instead of IQ, since there is no IQ path.

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
[IQ capture / efd-iq]
        │
        └─ broadcast<IqBlock>
               ├──→ [FFT / efd-dsp]    → FftBins  → [WS downstream] → clients
               └──→ [demod / efd-dsp]  → broadcast<AudioSamples>
                                               ├──→ [ALSA / efd-audio] → HAT/dongle → amp
                                               ├──→ [audio decoders / efd-dsp] → DecodedText
                                               └──→ [WS downstream]    → clients
```

## Tokio pipeline (RX path, MON + portable-radio modes)

```
[audio in / efd-audio]   (FDM-DUO USB audio or USB dongle analog-in)
        │
        └─ broadcast<AudioSamples>
               ├──→ [ALSA / efd-audio] → HAT/dongle → amp
               ├──→ [audio decoders / efd-dsp] → DecodedText → [WS downstream]
               ├──→ [audio FFT / efd-dsp] → FftBins (audio spectrum) → [WS downstream]
               └──→ [WS downstream]    → clients
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
- **PipeWire**: required on *client* machines for the virtual audio device that feeds digital-mode apps. Not required on CM5.
- **DREAM**: DRM decoder, runs on CM5. Audio output fed into demod audio path.
- **FreeDV**: codec, runs on CM5. Same audio path as DREAM.

---

## Key decisions (do not re-litigate without good reason)

| Topic | Decision |
|---|---|
| IQ over network | No. CM5 runs FFT, sends only magnitude bins to clients. |
| Audio codec (network) | Opus wideband, 48 kHz (not VoIP 8 kHz profile) |
| IQ backend abstraction | Vendor-native per device (no SoapySDR for now). Revisit later if the matrix grows. |
| Hardware CAT | `efd-cat` owns FDM-DUO USB serial directly. hamlib `rigctld` is not used — we speak full native CAT instead of hamlib's subset (FDM-DUO is the only source with hardware CAT, so coverage matters more than ecosystem compat at the wire level). |
| External-app CAT access | Hand-rolled rigctld-compatible responder in `efd-cat`, one codebase serving two bound ports with different backends: port A translates rigctld → native CAT for the FDM-DUO; port B fronts the software demod. Grown on demand as WSJT-X/FLDIGI/etc. require commands. |
| Two-responder model | When FDM-DUO is in SDR mode, both ports bind. Client sees one logical radio via merged RadioState + capability flags indicating hardware vs software. |
| DSP location | All DSP (FFT, demod, DRM, FreeDV, audio decoders) on CM5. Clients receive processed data only. |
| Audio-domain decoders | WEFAX/RTTY/CW/PSK operate on `AudioSamples` regardless of origin (IQ demod, FDM-DUO audio passthrough, or analog audio via dongle). |
| TX scope | RX-first. TX supported only on FDM-DUO and HackRF. Other configs advertise `has_tx=false`. |
| TX audio source | From client over WebSocket (mic, WSJT-X, FLDIGI, FreeDV via PipeWire). |
| Audio fan-out | In-process tokio broadcast channels. No PipeWire on CM5 backend. |
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
