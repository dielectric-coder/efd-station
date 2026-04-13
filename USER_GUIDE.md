# efd-station — User Guide

This guide is for people running an efd-station — either the full CM5 backend
with the GTK4 client, or just the client against someone else's server.

For the one-page overview see [`README.md`](README.md). For architectural
detail see [`CLAUDE.md`](CLAUDE.md).

---

## 1. What you need

### Radio / RF source (one of)

| Source | Role | Notes |
|---|---|---|
| Elad FDM-DUO | Primary target | IQ + audio + CAT all over USB |
| HackRF | Supported in code, untested | TX capable |
| RSPdx | Planned | RX only |
| RTL-SDR dongle | Planned | RX only |
| Portable analog radio | Audio-only mode | Feeds audio-domain decoders via a USB dongle |

Only the FDM-DUO path is fully exercised today. The others are wired into the
capability model but need runtime testing.

### Backend host

Raspberry Pi CM5 (Waveshare CM5-PoE-Base-A or similar). Debian Trixie or
compatible. Headless — no GUI required on the Pi.

### Audio output (standalone operation, optional for remote-only)

Either a Pi HAT sound card or a USB audio dongle. Exactly one, selected in
`config.toml`.

### Client machine

Any Linux box with GTK4 available. Network access to the Pi over LAN or WAN.

---

## 2. Installing the server on the CM5

### Debian (default for the CM5)

```bash
git clone https://github.com/dielectric-coder/efd-station
cd efd-station
./scripts/update-pi.sh
```

That script builds a `.deb` and installs it. It also drops a systemd service,
udev rules, and an example config.

### Arch / Manjaro

```bash
cd dist/arch
makepkg -sf
sudo pacman -U efd-server-*.pkg.tar.zst
```

### First-run setup (all distros)

```bash
# Copy the example config into place, then edit.
sudo cp /etc/efd-backend/config.toml.example \
        ~/.config/efd-backend/config.toml
```

Enable the service:

```bash
sudo systemctl enable --now efd-server
```

### Service-user prerequisite (DRM only)

The DRM decoder bridges to the vendored DREAM subprocess via PipeWire null
sinks, which requires the service to run under a user that has a live
PipeWire session. The shipped unit file runs as `mikel`. If you're deploying
under a different user, you need to:

1. `sudo loginctl enable-linger <user>` so `/run/user/$UID` persists.
2. Make sure `Environment=XDG_RUNTIME_DIR=/run/user/$UID` in the unit file.
3. Make sure `pactl`, `pacat`, `parec` are installed
   (`sudo apt install pulseaudio-utils` on Debian).
4. Confirm with `pactl info` run as that user — it should print
   `Server Name: PulseAudio (on PipeWire …)`.

`scripts/migrate-service-to-mikel.sh` automates this for the `mikel` case.

---

## 3. Configuration

Canonical path: `~/.config/efd-backend/config.toml`.

```toml
[server]
bind = "0.0.0.0"
port = 8080

[cat]
serial_device = "auto"     # auto-discovers the FDM-DUO CAT port
poll_interval_ms = 200

[dsp]
fft_size      = 4096
fft_averaging = 3
sample_rate   = 192000

[audio]
alsa_device = "default"    # RX playback (HAT or USB dongle — exactly one)
tx_device   = "default"    # TX back to FDM-DUO USB audio
sample_rate = 48000
```

Serial-device discovery tries, in order:

1. A `/dev/fdm-duo-cat` udev symlink (installed by the package).
2. `/dev/serial/by-id/` name matching.
3. Sysfs hub-sibling scan — finds the FTDI CAT port next to the Elad IQ
   device by shared USB hub parent.

---

## 4. Client

```bash
cargo run --release --package efd-client -- ws://<pi-host>:8080/ws
```

Prereq: GTK4 development libraries on the client machine.

- Debian/Ubuntu: `sudo apt install libgtk-4-dev`
- Arch: `sudo pacman -S gtk4`

A headless validator also ships, useful for confirming the pipeline without
touching the UI:

```bash
cargo run --example ws_test --package efd-client -- ws://<pi-host>:8080/ws
```

---

## 5. UI tour

- **Spectrum / waterfall** — top half. Shows the IQ spectrum in SDR mode,
  audio spectrum in MON/portable.
- **VFO readout** — top bar. Frequency, mode, bandwidth, S-meter / power,
  status flags.
- **DisplayBar extra rows** — two extra lines under the VFO. One reports
  DRM decoder flags (IO/Time/Frame/FAC/SDC/MSC) and service counts; the
  other reports SNR / WMER / MER / IF Level. Visible whenever the mode is
  `DRM`; blank otherwise.
- **Bottom bar** — SDR/MON toggle, AGC slider, frequency input, mode
  selector, tuning step, PTT, mute, volume.

Keyboard: spacebar for PTT (when `has_tx` is true for the active source).

---

## 6. Operating modes

### SDR mode (default for FDM-DUO, HackRF, RSPdx, RTL-SDR)

The backend captures IQ, demodulates in software, serves audio + spectrum
over WS. CAT reflects the software demod parameters. In this mode the FDM-DUO
can run its own hardware demod in parallel and neither interferes with the
other.

### MON mode (FDM-DUO only)

The radio demodulates. The backend just pipes its audio to ALSA and WS, and
reflects the radio's own CAT state. No software demod. Used when you want
the radio's built-in DSP — or a signal that software can't match yet.

### Portable-radio mode

Audio-only input from a USB dongle. No IQ, no CAT. Useful for running
audio-domain decoders (RTTY/PSK/CW/WEFAX …) over an unrelated analog
receiver.

### Mode list

| UI mode | Uses | Notes |
|---|---|---|
| USB / LSB | SDR or MON | Sideband SSB |
| AM | SDR or MON | Envelope detect |
| NFM | SDR or MON | FM discriminator |
| CW / CWR | SDR or MON | Tone-shifted SSB |
| DRM | **SDR only** | Wideband-SSB demod → vendored DREAM (audio-IF mode) — MON's narrow AM path destroys the 10 kHz OFDM block |
| FreeDV | SDR (planned) | Digital-voice codec |

---

## 7. DRM specifics

DRM (Digital Radio Mondiale) is the band's OFDM digital-broadcast standard.
efd-station decodes it by running a **wideband SSB demod** (10 kHz pass,
−5 kHz LSB to +5 kHz USB, real-valued audio-IF output with the DRM block
positioned around 12 kHz audio IF) and feeding that audio-IF stream into a
vendored **DREAM 2.1.1** subprocess via two PipeWire null sinks. DREAM
runs in its sound-card audio-IF mode — the same path used when feeding
DREAM's bundled FLAC samples manually with `paplay | dream`.

Requirements on the CM5:

- `pulseaudio-utils` installed (provides `pactl`, `pacat`, `parec`).
- The efd-server service runs under a user that has a working PipeWire
  session. See §2 for how that's enforced.
- Mode set to `DRM`. The mode must be SDR — MON-mode DRM isn't supported
  because the radio's built-in AM demod has a narrow passband that
  destroys the 10 kHz OFDM block before it reaches DREAM.
- Tune to a live DRM broadcast. Known quiet periods (overnight, off-season
  frequencies) will show strong spectrum but no decode.

When DREAM locks, the DisplayBar rows populate — FAC/SDC/MSC go from `✗` to
`O`, and you'll see SNR / WMER / IF Level numbers.

---

## 8. Troubleshooting

### No audio when tuning a signal

- Confirm the spectrum shows energy where you expect — if the spectrum is
  flat, the IQ capture is broken (check `journalctl -u efd-server` for USB
  errors).
- Standalone operation: check `aplay -l` on the CM5 to confirm the selected
  `alsa_device` exists.
- Remote operation: check the client's Mute toggle and Vol slider.

### `pactl load-module: No such file or directory`

`pulseaudio-utils` is not installed on the Pi. See §2.

### DRM stays `FAC:✗ SDC:✗ MSC:✗` despite a clean-looking signal

Either the signal isn't actually a DRM broadcast (common during off-air
periods on many frequencies), or the tuning is off — the wideband-SSB
demod pass needs the radio's center frequency within a few kHz of the
DRM carrier, so re-check your VFO setting against the known DRM channel.

### Client shows "connection refused"

- `systemctl status efd-server` — check the service is running.
- `ss -ltnp | grep 8080` on the Pi — confirm the port is bound.
- Firewall: `sudo ufw status` or equivalent; port 8080 must be reachable.

### CAT not controlling the radio

- Check `/dev/ttyUSB0` is present and owned by a group the service user can
  read (`dialout` by default).
- Confirm the radio's USB CAT speed matches `poll_interval_ms` expectations
  (FDM-DUO default: 38400 8N1).

### "ALSA snd_pcm_open failed" at startup

Another process has the audio device open. If you're running the client and
server on the same machine both pointing at the same `alsa_device`, that'll
fight.

---

## 9. Where next

- Feature bugs / requests: GitHub issues at
  <https://github.com/dielectric-coder/efd-station/issues>.
- Development: see [`DEV_GUIDE.md`](DEV_GUIDE.md).
- Architecture / design decisions: see [`CLAUDE.md`](CLAUDE.md).
