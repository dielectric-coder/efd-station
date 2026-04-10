# efd-station — CLAUDE.md

SDR application for the Elad FDM-DUO transceiver — CM5 backend + GTK4 native client,
single Cargo workspace. Repo: github.com/dielectric-coder/efd-station
This file is the authoritative project context. Update it as decisions are made.

---

## Hardware context

- **Radio**: Elad FDM-DUO, connected to CM5 via USB
  - Provides IQ stream (USB, used by efd-iq)
  - Provides audio capture (USB, used by efd-audio for TX path)
  - Receives TX audio (USB, from efd-audio)
  - CAT control via hamlib / rigctld (USB serial)
- **Host**: Raspberry Pi CM5 (headless, no GUI)
- **Audio output**: Pi HAT sound card → amplifier (ALSA, local playback)
- **Network**: Ethernet/WiFi — serves WebSocket clients on LAN or internet

---

## Operating modes

The same CM5 hardware supports two modes simultaneously:

1. **Standalone** — CM5 + FDM-DUO + HAT + amp. No network client needed.
   Audio goes directly to HAT → amp. Usable as a tabletop radio.

2. **Remote** — Any machine on the network runs the client UI.
   Receives FFT bins, audio stream, and radio state over WebSocket.
   Sends CAT commands, TX audio, and PTT over WebSocket.

---

## Workspace layout

```
<repo-root>/                    # single repo — server + client + shared crates
├── CLAUDE.md
├── Cargo.toml                  # workspace
├── server/                     # [[bin]] CM5 backend (Axum, tokio pipeline)
│   └── src/
│       ├── main.rs             # startup, config (~/.config/efd-backend/config.toml)
│       ├── pipeline.rs         # spawns all tasks, wires broadcast/mpsc channels
│       └── ws/
│           ├── downstream.rs   # server → client: FFT bins, audio, radio state
│           └── upstream.rs     # client → server: PTT, CAT commands, TX audio
├── client/                     # [[bin]] GTK4 native client
│   └── src/
│       ├── main.rs
│       ├── ui/                 # GTK4 widgets (dashboard, spectrum, waterfall, controls)
│       └── ws.rs               # WebSocket connection to server
└── crates/
    ├── efd-iq/                 # USB IQ capture from FDM-DUO
    ├── efd-dsp/                # FFT (FFTW3 + volk) + demodulation
    ├── efd-audio/              # ALSA HAT output + USB audio TX
    ├── efd-cat/                # rigctld TCP client + RadioState broadcast
    └── efd-proto/              # shared WS message types — used by server AND client
```

`efd-proto` is the contract between server and client. A breaking change to any
message type fails to compile both binaries simultaneously.

---

## Crate responsibilities

### efd-iq
- USB IQ capture from FDM-DUO
- Based on existing EladSpectrum code (github.com/dielectric-coder/EladSpectrum)
- Produces `IqBlock` items, published on a `tokio::sync::broadcast` channel
- No DSP here — raw IQ only

### efd-dsp
- **FFT task**: consumes `broadcast<IqBlock>`, produces magnitude bins
  - FFTW3 bindings + volk for SIMD-accelerated processing
  - Applies window function, computes magnitude spectrum
  - Publishes `FftBins` to a channel consumed by the WS downstream task
- **Demod task**: consumes `broadcast<IqBlock>`, produces `AudioSamples`
  - Modes: SSB, AM, FM, DRM (via DREAM), FreeDV
  - DREAM and FreeDV run on CM5 — decoded audio feeds the same audio path
  - Publishes `broadcast<AudioSamples>` for fan-out to ALSA and WS

### efd-audio
- **ALSA HAT task**: consumes `broadcast<AudioSamples>`, writes to HAT sound card
  - Provides standalone audio output to amp, no network needed
- **USB audio TX task**: consumes `mpsc<TxAudio>` from WS upstream, writes to FDM-DUO USB audio
  - TX audio originates from client mic, WSJT-X, FLDIGI, FreeDV, etc.
  - Client machines use PipeWire virtual audio device to feed digital mode apps

### efd-cat
- **rigctld TCP task**: connects to local rigctld daemon as a TCP client
  - rigctld daemon owns the FDM-DUO USB CAT port
  - WSJT-X and other digital mode apps also connect to rigctld as TCP clients
- Reads radio state (frequency, mode, filter, ATT, LP, AGC, etc.) from rigctld
- Publishes `broadcast<RadioState>` consumed by WS downstream
- Accepts `mpsc<CatCommand>` from WS upstream, proxies to rigctld

### efd-proto
- Shared serde types for all WebSocket messages
- **Server → client (downstream)**:
  - `FftBins` — magnitude bin array + metadata (center freq, span, ref level)
  - `AudioChunk` — encoded audio (Opus wideband 48 kHz)
  - `RadioState` — frequency, mode, BW, ATT, LP, AGC, NR, NB, S-meter, RX/TX
  - `Error`
- **Client → server (upstream)**:
  - `CatCommand` — frequency set, mode, BW, ATT, LP, AGC, NR, NB, RIT, XIT, etc.
  - `TxAudio` — encoded TX audio chunk
  - `Ptt` — PTT on/off

### server (binary)
- Axum HTTP + WebSocket server
- `pipeline.rs` spawns all tasks and wires channels:
  - `broadcast` channels for fan-out: IqBlock, AudioSamples, RadioState, FftBins
  - `mpsc` channels for single consumers: TxAudio → efd-audio, CatCommand → efd-cat
- WebSocket handler manages per-client state and routes messages

---

## Tokio pipeline (RX path)

```
[IQ capture / efd-iq]
        │
        └─ broadcast<IqBlock>
               ├──→ [FFT / efd-dsp]    → FftBins  → [WS downstream] → clients
               └──→ [demod / efd-dsp]  → broadcast<AudioSamples>
                                               ├──→ [ALSA / efd-audio] → HAT → amp
                                               └──→ [WS downstream]    → clients
```

## Tokio pipeline (TX + CAT path)

```
clients → [WS upstream] ──→ mpsc<TxAudio>    → [USB audio TX / efd-audio] → FDM-DUO
                        └──→ mpsc<CatCommand> → [rigctld task / efd-cat]
                                                        ↕ TCP
                                                   rigctld daemon
                                                        │
                                              broadcast<RadioState>
                                                        │
                                               [WS downstream] → clients
```

Channel type summary:
- `tokio::sync::broadcast` — IqBlock, AudioSamples, FftBins, RadioState (fan-out)
- `tokio::sync::mpsc` — TxAudio, CatCommand (single consumer)

---

## External dependencies (runtime, CM5)

- **rigctld** (hamlib): must be running on CM5, owns FDM-DUO USB CAT port.
  Backend and WSJT-X connect to it as TCP clients on localhost.
- **ALSA**: HAT audio output. No PipeWire required on CM5 for the backend itself.
- **PipeWire**: required on client machines to provide virtual audio device for
  digital mode apps (WSJT-X, FLDIGI, FreeDV). Not required on CM5.
- **DREAM**: DRM decoder, runs on CM5. Audio output fed into demod audio path.
- **FreeDV**: codec, runs on CM5. Same audio path as DREAM.

---

## Key decisions (do not re-litigate without good reason)

| Topic | Decision |
|---|---|
| IQ over network | No. CM5 runs FFT, sends only magnitude bins to clients. |
| Audio codec (network) | Opus wideband, 48 kHz (not VoIP 8 kHz profile) |
| CAT ownership | rigctld daemon on CM5. Backend + WSJT-X connect as TCP clients. |
| DSP location | All DSP (FFT, demod, DRM, FreeDV) on CM5. Clients receive processed data only. |
| TX audio source | From client over WebSocket (mic, WSJT-X, FLDIGI, FreeDV via PipeWire) |
| Audio fan-out | In-process tokio broadcast channels. No PipeWire on CM5 backend. |
| Language / framework | Rust, Axum, tokio async runtime |
| FFT library | FFTW3 + volk (from EladSpectrum) |
| WS serialization | bincode — both ends Rust, lowest overhead for FftBins at video rate. If a browser client ever needed, revisit MessagePack. |
| Client window host | GTK4 + GtkGLArea for OpenGL spectrum/waterfall. gtk4-rs bindings. |
| Config format | TOML |
| Config location | `~/.config/efd-backend/config.toml` |

---

## UI (client-side, separate project)

The client is a native app (not part of this workspace). Relevant context:
- Spectrum/waterfall rendering: glSpectrum via GtkGLArea (OpenGL)
- Window host: GTK4 (gtk4-rs)
- Receives FftBins → renders spectrum + waterfall
- Receives AudioChunk → decodes + plays back
- Receives RadioState → updates UI controls

UI wireframe controls (for reference when designing RadioState and CatCommand types):
- VFO A/B toggle, SPLIT, frequency (Hz), step, mode, BW
- S-meter (RX) / Power meter (TX), SNR (RX only)
- RIT on/off + delta (Hz), XIT on/off
- Memory store/recall
- ATT (on/off toggle), LP 50 MHz (on/off toggle)
- NR (popup), NB (popup), AGC (popup), AGC threshold, auto-notch (on/off)
- CW WPM, CW tone (Hz)
- PTT (toggle, spacebar)
- Ref level (dBm), span (kHz), WF speed, WF palette
- Spectrum panel + waterfall panel (freq axis + filter passband overlay)

---

## Pending decisions

- [ ] FftBins precision: f32 vs f16 (display needs ~11 bits of mantissa; halves bin payload ~4 KB → ~2 KB at 1024 bins/frame)
- [ ] FftBins encoding: raw array vs delta/compressed
- [ ] Audio chunk size / latency budget for remote operation
- [ ] TLS / authentication for remote WebSocket clients
- [ ] PTT sequencing: who arbitrates PTT when digital mode app (WSJT-X) and
      client UI both have PTT capability?

---

## Repo

github.com/dielectric-coder/efd-station
Related: github.com/dielectric-coder/EladSpectrum (efd-iq code to reuse)
