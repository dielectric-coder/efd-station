# efd-station — Developer Guide

This guide is for people hacking on the codebase. For architectural decisions
and cross-crate contracts, [`CLAUDE.md`](CLAUDE.md) is authoritative — this
guide is how-to-do-things, not how-things-work.

For end-user setup see [`USER_GUIDE.md`](USER_GUIDE.md).

---

## 1. Repo tour

```
<repo-root>/
├── CLAUDE.md                   # authoritative architecture + decisions
├── USER_GUIDE.md               # end-user walkthrough
├── DEV_GUIDE.md                # this file
├── CHANGELOG.md                # per-version changes
├── README.md                   # landing page
├── PROGRESS.md                 # running dev log
├── Cargo.toml                  # workspace
├── server/                     # [[bin]] Axum + tokio pipeline
├── client/                     # [[bin]] GTK4 native client
├── crates/
│   ├── efd-iq/                 # Multi-backend IQ capture (trait + drivers/*)
│   ├── efd-dsp/                # FFT, analog demod, DRM bridge, audio decoders
│   ├── efd-audio/              # ALSA in/out + USB audio TX
│   ├── efd-cat/                # CAT serial + rigctld-compatible responder
│   └── efd-proto/              # Shared WS types (serde/bincode)
├── third_party/
│   └── dream/                  # vendored DREAM 2.1.1 (DRM decoder)
├── dist/                       # Packaging (Arch PKGBUILD, systemd, udev, example config)
├── docs/                       # Architecture diagrams (drawio + svg)
└── scripts/                    # Build/deploy helpers
```

`efd-proto` is the contract between server and client. Break a message there
and both binaries fail to compile simultaneously — by design.

See `CLAUDE.md` §Crate responsibilities for the authoritative breakdown of
what each crate owns.

---

## 2. Dev environment

### Prerequisites

```bash
# Debian / Raspberry Pi OS
sudo apt install build-essential libusb-1.0-0-dev libasound2-dev \
                 libopus-dev libgtk-4-dev pkg-config

# Arch / Manjaro
sudo pacman -S rust libusb alsa-lib opus gtk4 pkg-config
```

Rust: current stable. The workspace uses edition 2021.

### DREAM (DRM decoder) build

Vendored under `third_party/dream/`. Needs `qt6-base-dev` (or `qt5-base-dev`)
and modern hamlib dev headers. Build once with its script:

```bash
third_party/dream/build.sh
```

Only relevant if you're touching DRM or deploying a fresh machine — for
normal Rust dev you can ignore it.

---

## 3. Build / test / run

```bash
# Full workspace build
cargo build --workspace

# Release (what the CM5 runs)
cargo build --release --package efd-server

# Unit + integration tests
cargo test --workspace

# Just one crate
cargo test -p efd-dsp

# Run the server locally (uses ~/.config/efd-backend/config.toml)
cargo run --package efd-server

# Run the client against a running server
cargo run --package efd-client -- ws://localhost:8080/ws

# Headless WS validator (good for pipeline debugging)
cargo run --example ws_test --package efd-client -- ws://localhost:8080/ws
```

### Feature flags worth knowing

- `efd-iq` has a per-driver feature scheme. `fdm-duo` is the only driver
  today and is on by default. Building `efd-iq` with `--no-default-features`
  yields a driverless crate — useful for hosts without rusb, and the model
  future HackRF/RSPdx/RTL drivers will follow.

---

## 4. Debug tips

### Logs

```bash
# Everything at info
RUST_LOG=info cargo run --package efd-server

# Turn up one subsystem
RUST_LOG=info,efd_dsp::drm=debug cargo run --package efd-server
```

On the CM5 (systemd service):

```bash
journalctl -u efd-server -f
journalctl -u efd-server --since '10 min ago' --no-pager | grep -iE 'DRM|dream|bridge'
```

### DRM bridge diagnostics

The bridge already publishes a TUI activity line every 5 s at `info`
level:
```
efd_dsp::drm: DRM TUI: activity lines_read=… frames_published=…
```
If `lines_read` climbs but `frames_published` doesn't, DREAM is
outputting but our `parse_tui_line` match for the frame terminator
(`"Received time - date:"`) isn't firing — probably a DREAM version
change. If neither climbs, DREAM isn't writing to our piped stdout at
all: likely a stale DREAM build without the `0002-consoleio-stdout-
fallback` patch, or `setsid` isn't taking effect and DREAM has found a
controlling tty to write to.

`crates/efd-dsp/src/drm.rs` spawns DREAM with `stderr(Stdio::null())` —
if you're debugging deeper DRM lock failures, swap that to
`Stdio::piped()` temporarily and tee it to a file. Don't commit that
change.

### Client UI without the backend

`examples/ws_test` is the fastest feedback loop for WS-side debugging. For
UI work without a live radio, the client has mock-data plumbing in `client/`
— grep for `mock_` to find the affordances.

### DRM chain without a radio

```bash
EFD_DRM_FILE_TEST=third_party/dream/samples/VoiceOfRussia_ModeB_10kHz.flac \
  cargo run --release -p efd-server
```

Server picks a minimal pipeline (`Pipeline::start_drm_file_test` in
`server/src/pipeline.rs`): no IQ capture, no demod, no CAT, no FFT.
The DRM bridge spawns directly in `DrmInput::File(path)` mode — DREAM
reads the file natively via its `-f` flag (libsndfile for WAV/FLAC,
extension-sniffed for raw `.iq`/`.if`/`.pcm`). No Rust-side file
reader, no `drm_in` null sink, no `pacat`. Only the output side
(DREAM → `drm_out.monitor` → parec → Opus → WS) is plumbed, and it's
the same code path as production so a real `efd-client` connected to
`ws://localhost:8080/ws` exercises the full client-side chain.

A synthetic `RadioState { mode: DRM, bw: "10.0k" }` is emitted every
500 ms so the client gates its DRM display rows on. Capabilities are
advertised as `has_iq=false, has_tx=false, supported_demod_modes=[DRM]`.
On FLAC EOF the pipeline fires its cancel token and the server exits.

Useful for: validating DREAM subprocess wiring after a refactor,
confirming the client renders DRM status correctly, reproducing an
audio-chopping bug without tying up the radio.

If a sample file has inverted spectrum (one of DREAM's bundled samples
is `R_Nigeria_Mode_C_10kHz_flipped_spectrum.flac`), set
`[drm] flip_spectrum = true` in `config.toml` so DREAM is launched with
`-p`.

**Runtime deps**: vendored DREAM built via `third_party/dream/build.sh`,
`pulseaudio-utils`, `libfaad2` (DREAM dlopens it for AAC decode;
without it DREAM locks cleanly but produces silence).

---

## 5. Extending

### 5a. Adding a new IQ driver

1. Add a feature flag in `crates/efd-iq/Cargo.toml`:
   ```toml
   [features]
   default = ["fdm-duo"]
   fdm-duo = ["dep:rusb"]
   hackrf  = ["dep:libhackrf-sys"]   # example
   ```
2. Add the vendor dep as `optional = true`.
3. Create `crates/efd-iq/src/drivers/<name>.rs` with a `spawn(...)` entry
   point matching the FDM-DUO driver's signature:
   ```rust
   pub fn spawn(
       cfg: <YourConfig>,
       tx: broadcast::Sender<Arc<IqBlock>>,
       center_freq_tx: watch::Sender<u64>,
       cancel: CancellationToken,
   ) -> JoinHandle<Result<(), IqError>>
   ```
4. Add the feature-gated `pub mod <name>;` line in `drivers/mod.rs`.
5. Add the dispatch arm in `lib.rs::spawn_source` under the matching
   `#[cfg(feature = …)]`.
6. If the driver has new failure modes, extend `IqError` (gate any
   vendor-typed variants behind the feature).
7. Add the source to `efd-proto::SourceKind` and update the match in
   `efd-iq::source::SourceConfig::capabilities()` with its capability bits.

When a *second* driver actually lands, extract the common shape into an
`IqSource` trait. Until then, the free-function dispatch is deliberate —
one impl does not justify an abstraction.

### 5b. Adding an analog demod mode

All analog IQ demods share one task in `efd-dsp::demod` with a mode
parameter (see `CLAUDE.md` Tier 1). To add (say) SAM:

1. Add the variant to `efd-proto::Mode`.
2. Extend `efd-dsp::demod::mode::AnalogMode` (or equivalent) with the new
   variant.
3. Add its filter shape to `demod/filter.rs` and its detector logic
   (`demod/detector.rs`) — PLL for SAM, envelope for AM, FM discriminator,
   etc.
4. Update `efd-iq::source::SourceConfig::capabilities()` to include the new
   mode in `supported_demod_modes` for sources that can do it.
5. Client: grow the mode selector in `client/src/ui/controls.rs`.
6. Unit-test the detector with a synthetic signal.

Mode-switches within Tier 1 should reconfigure the existing task, not
respawn it — see the state machine in `server/src/pipeline.rs`.

### 5c. Adding an IQ-domain codec (Tier 2)

DRM is the template. Add a new module under `crates/efd-dsp/src/codec/` with
the same shape as `drm.rs`:

- Consume `broadcast<IqBlock>`.
- Produce `broadcast<AudioSamples>` via whatever path the codec needs
  (in-process or subprocess + PipeWire bridge).
- Expose a `spawn(...)` function.
- Handle full teardown on mode change — Tier-2 codecs are mutually
  exclusive and with Tier-1, no reuse.

Wire it into the mode state machine in `server/src/pipeline.rs`.

### 5d. Adding an audio-domain decoder (Tier 3)

Audio-domain decoders are always-on and mode-agnostic — they consume
`broadcast<AudioSamples>` regardless of where the audio came from. To add:

1. New file `crates/efd-dsp/src/decoder/<name>.rs`.
2. Input: an `AudioSamples` receiver. Output: typed decoder events to WS
   downstream (usually `efd_proto::DecodedText` or a dedicated variant).
3. Register in `decoder::registry` so config-driven enable/disable works:
   `decoder.enabled = ["cw", "ft8"]`.
4. Add a `supported_audio_decoders` entry to `efd-proto::Capabilities` if
   you want the client to grey it out on unsupported sources.
5. Client: add a decoder-output panel in `client/src/ui/` if the decoder
   produces human-readable output.

### 5e. Adding a rigctld command to the responder

External apps (WSJT-X, FLDIGI, etc.) reach the radio or demod through the
hand-rolled rigctld-compatible responder in `crates/efd-cat/src/responder/`.
To add a command:

1. Extend the parser to recognize the new rigctld command.
2. Implement two handlers — one for the FDM-DUO-front port (translates to
   native FDM-DUO CAT), one for the demod-front port (translates to an
   internal demod command).
3. Add a test in `crates/efd-cat/tests/` covering wire format + both
   handlers.

Grow on demand — what WSJT-X / FLDIGI actually use, in the order they need
it. No need to implement rigctld in full.

### 5f. Changing `efd-proto`

The contract between server and client. Rules of the road:

- Any breaking change to a struct fails both binaries to compile — that's
  the feature, not a bug. Fix both sides in the same commit.
- Additive changes (new enum variant, new optional field) are backward
  compatible at the bincode level *if* the new field is added at the end
  and defaulted. Prefer explicit `#[serde(default)]` on new `Option<T>`
  fields.
- Update `efd-proto::Capabilities` whenever a new capability bit is added
  so older clients can degrade gracefully.

---

## 6. Deployment

### Update a running CM5 from a local repo

```bash
# On the Pi:
git pull
./scripts/update-pi.sh             # full build + .deb + dpkg -i
./scripts/update-pi.sh --quick     # just rebuild, skip packaging
```

### First-time migration (or redeploy under a new user)

`scripts/migrate-service-to-mikel.sh` is idempotent and handles:

- `loginctl enable-linger` for the target user
- adding `dialout/audio/plugdev` group membership
- copying the backend config to the target user's home
- installing the updated unit file
- sanity-checking PipeWire reachability via `pactl info` before restart

### Packaging (Arch)

```bash
cd dist/arch
makepkg -sf
sudo pacman -U efd-server-*.pkg.tar.zst
```

`PKGBUILD`'s `pkgver` must match `server/Cargo.toml`'s `version`. The
checklist in §7 covers this.

---

## 7. Conventions

### Commit style

Look at `git log` — the repo uses Conventional Commits loosely:

- `feat:` new feature
- `fix:` bug fix
- `refactor:` change without behavior impact
- `docs:` docs only
- `test:` tests only
- `client(ui):` / `server(…):` / `efd-cat:` scopes are used where the
  area is obvious

Body should say **why**, not what — the diff says what.

No Co-Authored-By lines from AI tools.

### Version bumps + CHANGELOG

When a change is significant (touches the server binary behavior, modifies
the shipped unit file, changes a deployable contract):

1. Bump `server/Cargo.toml` `version`.
2. Bump `dist/arch/PKGBUILD` `pkgver` to match.
3. Add an entry to `CHANGELOG.md` with sections `### Added / Changed /
   Fixed / Removed / Docs` as applicable.
4. Commit the whole lot together.

For in-flight work that's not yet released, append to the `[Unreleased]`
section at the top of `CHANGELOG.md`. When you cut a version, rename
`[Unreleased]` to the new version header with today's date.

### Tests

- Unit tests next to the code in `#[cfg(test)] mod tests { … }`.
- Integration tests under `crates/<c>/tests/` — use these for anything
  that crosses a channel boundary or spawns a subprocess.
- Protocol / wire-format tests live in `efd-cat/tests/` (rigctld) and
  `efd-proto/tests/` (bincode round-trips).

### Docs

- `CLAUDE.md` is the authoritative architecture reference. Keep it updated
  when decisions change.
- `USER_GUIDE.md` and `DEV_GUIDE.md` are how-to documents. Don't duplicate
  `CLAUDE.md` — link to it.
- `README.md` is the landing page. Keep it short and pointing at the
  other three.

### Not-yet-applicable / hypothetical features

Don't add code, abstractions, or empty feature flags for things that aren't
needed yet. The `IqSource` trait is the canonical example — it's prescribed
in `CLAUDE.md` as the target abstraction but won't be introduced until
driver #2 actually exists.

---

## 8. Pointers

- Architecture & decisions: [`CLAUDE.md`](CLAUDE.md)
- End-user setup: [`USER_GUIDE.md`](USER_GUIDE.md)
- Per-version changes: [`CHANGELOG.md`](CHANGELOG.md)
- Running dev log: [`PROGRESS.md`](PROGRESS.md)
- Architecture diagrams: [`docs/CM5-sdr-backend.drawio`](docs/CM5-sdr-backend.drawio)
- Related repo (FDM-DUO IQ reference): <https://github.com/dielectric-coder/EladSpectrum>
