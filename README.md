# efd-station

A multi-SDR ham-radio station: networked transceiver backend for the **Elad FDM-DUO**, extensible to **HackRF**, **SDRplay RSPdx**, **RTL-SDR**, and a plain analog portable radio. Runs headless on a Raspberry Pi CM5; native GTK4 client on any Linux desktop.

All DSP — FFT, demodulation, DRM, FreeDV, audio-domain decoders — happens on the CM5. Clients receive only processed data (magnitude bins, Opus audio, radio state).

```
[SDR / radio] ──USB──▶ [CM5 backend] ──WebSocket──▶ [GTK4 client]
   IQ / audio             FFT · demod · codecs         spectrum · waterfall
   CAT serial             rigctld responder            audio · controls · PTT
                          ALSA → HAT/amp (standalone)
```

## Status

Work in progress. The architecture is settled (see `CLAUDE.md`); crates are landing incrementally — FDM-DUO IQ, CAT, audio, and the WS pipeline first; other backends follow behind cargo feature flags.

## Features

- **Multi-backend IQ capture** — one `IqSource` trait, one driver file per device, each behind a cargo feature. Adding a new SDR = one file + one feature + one factory arm. Vendor-native (no SoapySDR).
- **Two operating modes per source**
  - **MON** — FDM-DUO as a conventional receiver; audio from the radio, CAT reports hardware state.
  - **SDR** — any supported source feeds IQ into the software demod; the demod fronts as a radio via a rigctld-compatible endpoint.
- **Three-tier DSP**
  1. Analog IQ demod — one task, mode param (AM / SAM / USB / LSB / CW± / NFM / WFM, plus a wideband-SSB configuration that produces a 10 kHz audio-IF stream for DRM instead of listenable audio).
  2. Codecs — DRM (vendored DREAM 2.1.1 subprocess; Tier-1's wideband-SSB audio-IF is bridged into DREAM via PipeWire null sinks), FreeDV.
  3. Audio-domain decoders — CW / RTTY / PSK / WSPR / FT8 / APRS / WEFAX, N in parallel, mode-agnostic, works in MON and portable configs too.
- **Hand-rolled rigctld responder** inside `efd-cat` — external apps (WSJT-X, FLDIGI, digital-mode tooling) connect to our TCP endpoint instead of fighting for the FDM-DUO USB serial port. hamlib's `rigctld` is not used on the CM5; we speak the FDM-DUO's full native CAT.
- **Standalone or remote** — CM5 + HAT sound card + amp works as a tabletop radio with no network client; any number of GTK4 clients can also be connected simultaneously.
- **TX** on FDM-DUO and HackRF; per-device capability flags tell the client what to enable or grey out.

## Supported RF sources

| Source          | IQ path              | Audio path            | Hardware CAT | SW demod | TX  |
|-----------------|----------------------|-----------------------|--------------|----------|-----|
| FDM-DUO (MON)   | —                    | FDM-DUO USB audio     | native (USB) | no       | yes |
| FDM-DUO (SDR)   | FDM-DUO USB IQ       | from SW demod         | native (USB) | yes      | yes |
| HackRF          | libhackrf            | from SW demod         | —            | yes      | yes |
| RSPdx           | SDRplay API          | from SW demod         | —            | yes      | no  |
| RTL dongle      | librtlsdr            | from SW demod         | —            | yes      | no  |
| Portable radio  | —                    | USB dongle analog-in  | —            | no       | no  |

## Architecture at a glance

Single Cargo workspace:

| Crate        | Purpose |
|--------------|---------|
| `efd-proto`  | Shared WS message types (bincode). Breaking changes fail both binaries at compile time. |
| `efd-iq`     | Multi-backend IQ capture. Trait + per-device drivers (`fdm_duo`, `hackrf`, `rspdx`, `rtl`), feature-gated. |
| `efd-dsp`    | FFT (FFTW3 + volk), Tier-1 analog demod (incl. wideband-SSB DRM feed), Tier-2 codecs (DRM, FreeDV), Tier-3 audio decoders. |
| `efd-audio`  | ALSA HAT/USB output, analog audio-in for MON/portable, USB audio TX. |
| `efd-cat`    | Direct USB serial CAT to FDM-DUO + rigctld-compatible TCP responder (two bindable ports: hardware front, demod front). |
| `server`     | Axum HTTP/WebSocket server, tokio pipeline, MON/SDR state machine. |
| `client`     | GTK4 + GtkGLArea spectrum/waterfall, controls, PTT. |

See **`CLAUDE.md`** for the full architecture (pipeline diagrams, channel layout, decision log) and **`docs/CM5-sdr-backend.drawio.svg`** for the visual overview.

## Hardware

- **Host**: Raspberry Pi CM5 on a Waveshare CM5-PoE-Base-A IO board, headless.
- **Audio out**: Pi HAT sound card or USB audio dongle → amplifier. Exactly one per config.
- **Network**: Ethernet / WiFi.
- **GPIO front panel**: reset / lock / status LEDs only. Not used for tuning or CAT.

## Building

Rust stable, Cargo workspace. Per-backend features keep deps off hosts that don't need them (a CM5 without SDRplay headers simply omits `--features rspdx`).

```bash
git clone https://github.com/dielectric-coder/efd-station.git
cd efd-station

# FDM-DUO only (default on the CM5 target):
cargo build --release -p efd-server --features fdm-duo

# Multiple backends:
cargo build --release -p efd-server --features "fdm-duo hackrf rtl"
```

Runtime deps on the CM5: ALSA, PipeWire (for DRM null-sink bridge), vendored DREAM 2.1.1 under `third_party/dream/` (built `qmake CONFIG+=console` with a one-line hamlib cast patch). `hamlib rigctld` is **not** required.

## Running

```bash
efd-server                               # direct
sudo systemctl enable --now efd-server   # via systemd
RUST_LOG=debug efd-server                # with trace logging
```

WebSocket endpoint: `ws://host:8080/ws`

## Client

```bash
cargo run -p efd-client -- ws://pi-hostname:8080/ws
```

The client renders spectrum and waterfall (IQ spectrum in SDR mode, audio spectrum in MON/portable), plays Opus audio, shows radio state, sends CAT/PTT/TX-audio upstream, and enables controls based on the source's advertised capabilities.

## Configuration

TOML, at `~/.config/efd-backend/config.toml`. The active source, audio output device, enabled audio decoders, and responder ports are all selected here. Full schema lives alongside the crates — see `docs/DEV_GUIDE.md` and `docs/USER_GUIDE.md`.

## Docs

- `CLAUDE.md` — authoritative architecture + decision log
- `docs/USER_GUIDE.md` — setup, configuration, operation
- `docs/DEV_GUIDE.md` — building, contributing, crate internals
- `docs/CM5-sdr-backend.drawio.svg` — architecture diagram

## License

**efd-station** — a multi-SDR ham-radio station for the Raspberry Pi CM5.
Copyright (C) 2026 dielectric-coder

SPDX-License-Identifier: GPL-3.0-or-later

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.

This program is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with
this program. If not, see <https://www.gnu.org/licenses/>.

The full license text is also available in the [`LICENSE`](LICENSE) file at
the root of this repository.

### Third-party components

- **DREAM 2.1.1** (vendored under `third_party/dream/`) — GPL-2.0-or-later,
  © Volker Fischer and contributors. Used as a separately-invoked subprocess.
- **FFTW3**, **volk**, **libhackrf**, **librtlsdr**, **hamlib** — linked per
  enabled cargo feature; each retains its own license (GPL / LGPL / BSD as
  upstream declares).
- **SDRplay API** — proprietary, redistributed under SDRplay's own license
  terms; only built into the binary when the `rspdx` feature is enabled.
