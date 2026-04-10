# efd-station

SDR backend and client application for the **Elad FDM-DUO** transceiver, running on a Raspberry Pi CM5.

## Overview

efd-station turns an Elad FDM-DUO into a networked SDR. The CM5 backend captures IQ data over USB, runs all DSP locally (FFT, demodulation, encoding), and serves processed data to clients over WebSocket. The radio can operate standalone (CM5 + HAT sound card + amp) or remotely (any machine on the network running the client).

```
[FDM-DUO] ──USB──> [CM5 backend] ──WebSocket──> [Client UI]
  IQ data              FFT, demod, Opus             spectrum, waterfall,
  CAT serial           ALSA local playback          audio, controls
  USB audio            radio state polling
```

## Architecture

Single Cargo workspace:

| Crate | Purpose |
|---|---|
| `efd-proto` | Shared WS message types (bincode serialization) |
| `efd-iq` | USB IQ capture from FDM-DUO via libusb |
| `efd-dsp` | FFT (rustfft, Blackman-Harris, 4096-point) + AM/USB/LSB/FM demodulation |
| `efd-audio` | ALSA playback, Opus wideband encode/decode, USB TX audio |
| `efd-cat` | Direct serial CAT control (38400 8N1), auto-discovery |
| `server` | Axum HTTP/WS server, tokio pipeline wiring |
| `client` | GTK4 native client: spectrum, waterfall, controls, PTT |

### Data flow

**RX path:**
```
IQ capture → broadcast<IqBlock> → FFT → FftBins → WS → clients
                                → demod → Opus encode → WS → clients
                                                      → Opus decode → ALSA → amp
```

**TX + CAT path:**
```
clients → WS → CatCommand → serial port → FDM-DUO
              → TxAudio → Opus decode → USB audio TX → FDM-DUO
              → Ptt → TX;/RX; → serial port
```

## Building

### Prerequisites

```bash
# Debian/Raspberry Pi OS
sudo apt install build-essential libusb-1.0-0-dev libasound2-dev libopus-dev pkg-config

# Arch/Manjaro
sudo pacman -S rust libusb alsa-lib opus pkg-config
```

### Build

```bash
git clone https://github.com/dielectric-coder/efd-station.git
cd efd-station
cargo build --release --package efd-server
```

### Install packages

**Debian/Raspberry Pi OS (.deb):**
```bash
cargo install cargo-deb
cargo deb --package efd-server
sudo dpkg -i target/debian/efd-server_*.deb
```

**Arch/Manjaro:**
```bash
cd dist/arch
makepkg -sf
sudo pacman -U efd-server-*.pkg.tar.zst
```

Both packages install a systemd service, udev rules, and example config.

### Client

The GTK4 client runs on any Linux machine with a display:

```bash
# Install GTK4 dev (if not present)
# Debian: sudo apt install libgtk-4-dev
# Arch: sudo pacman -S gtk4

cargo run --package efd-client -- ws://pi-hostname:8080/ws
```

**Headless test client** (no GUI, validates the pipeline):
```bash
cargo run --example ws_test --package efd-client -- ws://pi-hostname:8080/ws
```

## Configuration

Config file: `~/.config/efd-backend/config.toml`

```toml
[server]
bind = "0.0.0.0"
port = 8080

[cat]
serial_device = "auto"    # auto-discovers FDM-DUO CAT port
poll_interval_ms = 200

[dsp]
fft_size = 4096
fft_averaging = 3
sample_rate = 192000

[audio]
alsa_device = "default"   # RX playback (HAT sound card)
tx_device = "default"     # TX audio to FDM-DUO USB audio
sample_rate = 48000
```

Serial device discovery tries (in order):
1. `/dev/fdm-duo-cat` udev symlink
2. `/dev/serial/by-id/` name matching
3. Sysfs hub-sibling scan (finds FTDI serial port next to Elad IQ device)

## Running

```bash
# Direct
efd-server

# Via systemd
sudo systemctl enable --now efd-server

# With debug logging
RUST_LOG=debug efd-server
```

Health check: `curl http://localhost:8080/health`

WebSocket endpoint: `ws://host:8080/ws`

## Hardware

- **Radio**: Elad FDM-DUO (3 USB interfaces: IQ data, audio, CAT serial)
- **Host**: Raspberry Pi CM5 (headless)
- **Audio output**: Pi HAT sound card → amplifier (ALSA)
- **Network**: Ethernet/WiFi — serves WS clients on LAN or internet

## License

SPDX-License-Identifier: GPL-3.0-or-later

Copyright (C) 2026 dielectric-coder

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with
this program. If not, see <https://www.gnu.org/licenses/>.
