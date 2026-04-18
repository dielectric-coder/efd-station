# efd-station — Rework architecture

Reference for the planned server + client rework. The authoritative
visual sources are the six drawio files under `docs/`; this document
is the written companion so anyone can grok the intent without
opening draw.io.

- **Baseline before rework**: git tag `pre-major-rework` at `40c7151`
  (server `0.6.14`, client `0.5.3`, all five review-batch hardenings
  applied).
- **Source diagrams** (in `docs/`):
  - `CM5-sdr-backend-hardware.drawio` — physical topology
  - `CM5-sdr-backend-flowchart.drawio` — runtime state machine
  - `CM5-sdr-backend-pipeline-AUD.drawio` — portable / USB-audio
    passthrough mode
  - `CM5-sdr-backend-pipeline-IQ-NO-DRM.drawio` — IQ mode, analog
    / digital audio modes (AM, SAM, SSB, CW, FM, etc.)
  - `CM5-sdr-backend-pipeline-IQ-DRM.drawio` — IQ mode, live DRM
  - `CM5-sdr-backend-pipeline-FLAC-DRM.drawio` — recorded-file DRM
    (`dream -f`)
  - `client-sdr-UI.drawio` — GTK client UI grid + IQ-NO-DRM example

The prior doc `CM5-sdr-backend-pipeline.drawio` and the matching
`project_pipeline_refactor` memory entry reflect an earlier plan.
Where it conflicts with what's below, **this document wins**.

---

## 1. Hardware topology

Five RF sources feed the CM5 through its four USB ports and its
audio HAT:

| Source          | Audio path        | IQ path          | CAT                  |
|-----------------|-------------------|------------------|----------------------|
| Portable radio  | analog → USB dongle (or HAT mic-in) | —                | —                    |
| FDM-DUO         | USB audio         | USB IQ (native)  | USB serial (native)  |
| RSPdx           | —                 | USB (SDRplay API)| —                    |
| HackRF          | —                 | USB (libhackrf)  | —                    |
| RTL dongle      | —                 | USB (librtlsdr)  | —                    |

USB budget and combinations supported out of the box:

- 1 × FDM-DUO + 1 × SDR, or
- up to 4 × SDRs if you forgo CAT and USB audio, or
- analog-audio sources via USB-audio dongles on any free port.

CM5 IO board (Waveshare CM5-PoE-Base-A) also carries:

- **Audio HAT + DSP** — local speaker/amp out
- **Network PHY** — Ethernet / PoE
- **GPIO** — minimal: reset button, lock, status LEDs. No tuning
  or CAT via GPIO.

The **CM5 module** itself runs the backend server. Its role in
one sentence: ingest IQ or audio, produce audio-out plus text/flag
output plus CAT, all over the network.

---

## 2. Runtime state machine

The backend is no longer a "read config, commit to one source,
never look back" pipeline. Per `CM5-sdr-backend-flowchart.drawio`:

```
start
  │
  ▼
 Saved state?
  │
  ├─ yes ──▶ scan to validate saved state
  │           │
  │           ▼
  │          valid?
  │           │
  │           ├─ yes ──▶ (operate / select available devices)
  │           │
  │           └─ no  ──▶ fall through to fresh scan
  │
  └─ no ───▶ scan audio input devices
             │
             ▼
             scan serial ports
             │
             ▼
             scan IQ USB devices
             │
             ├─ IQ?     → start acquisition from 1st IQ device
             └─ Audio?  → start acquisition from 1st audio device
             │
             ▼
           (operate / select available devices)
             │
             ▼
           Quit?
             │
             ├─ no  ── loop
             └─ yes ── save state and exit
```

Three new responsibilities for the backend:

1. **Device discovery at startup** — enumerate audio-in, serial,
   and IQ-USB devices; do not rely on a config pin.
2. **Persistent saved state** — on quit, serialise the current
   selection + tuning (freq, mode, BW, RIT, XIT, IF, decoder
   selection, filter toggles). On next start, validate against
   the current device list; fall back to fresh discovery on
   mismatch.
3. **Runtime source / device swap** — the "operate / select
   available devices" hub must support swapping the active
   source (audio ↔ IQ) or the active device (FDM-DUO ↔ RTL ↔ …)
   without a restart.

Today's config-pinned `~/.config/efd-backend/config.toml` stays
as a hint / defaults file, but the live selection lives in
persisted state, not in the config.

---

## 3. Pipeline topologies

The four pipeline diagrams share a single canvas — boxes are
coloured green for the active path in the mode each diagram
depicts. Read left-to-right, the canvas has these boxes:

```
hardware domain          │                backend domain
─────────────────────────┤─────────────────────────────────────────────────────
Audio source ─────────┐  │
                      │  │
IQ source → NB → IQ→IF │ →  IF demod ──┬─→ DRM ──┐
                      │  │             │         │
                      │  │             └─────────┼─→ digital decode ──→ text/flag out ──→ Network
                      │  │                       │
FLAC/WAV file source ─┼──┼────────────────────── ┤
                      │  │                       │
                      └──┼───────────────────────┴─→ DSP (DNB DNR DNF APF) ──→ Audio Out ──→ Network
                         │
```

### 3.1 AUD mode (portable radio or FDM-DUO USB-audio passthrough)

Active:

- `Audio source → DSP → Audio Out`
- `Audio source → digital decode → text/flag out`

Inactive: IQ source, NB, IQ→IF, IF demod, DRM, FLAC file source.

### 3.2 IQ-NO-DRM mode (live analog or audio-domain digital modes)

Active:

- `IQ source → NB → IQ→IF → IF demod → DSP → Audio Out`
- `IF demod → digital decode → text/flag out`

Inactive: Audio source, DRM, FLAC file source.

### 3.3 IQ-DRM mode (live DRM decoding)

Active:

- `IQ source → NB → IQ→IF → IF demod → DRM → DSP → Audio Out`
- `DRM → digital decode → text/flag out`

The IF demod produces a **single USB demodulation**, offset
−5 kHz, 10 kHz bandwidth, which is handed to DREAM via the
existing PipeWire null-sink bridge. DREAM is **not** launched
with `-f` in this mode; `-f` is exclusive to the FLAC path
below. This supersedes the current CLAUDE.md phrasing that
describes a LSB+USB dual passband.

### 3.4 FLAC-DRM mode (recorded-file DRM decoding)

Active:

- `FLAC/WAV file source → DRM (dream -f) → DSP → Audio Out`
- `DRM → digital decode → text/flag out`

Inactive: Audio source, IQ source chain, IF demod.

DREAM opens the file itself via libsndfile (`-f`). No Rust-side
file reader, no `drm_in` null sink, no `pacat` needed on the
input side. This is the current `EFD_DRM_FILE_TEST` path,
promoted to a first-class mode.

### 3.5 Key differences from today's code

- **NB moves pre-IF** (on raw IQ), not audio-domain.
- **DSP** is a named block with exactly four audio-domain
  filters: DNB (digital noise blanker), DNR (digital noise
  reduction), DNF (digital notch filter), APF (audio peak
  filter). Each toggleable independently.
- **DRM IF feed** is single-USB (−5 kHz offset, 10 kHz BW), not
  LSB+USB.
- **FLAC file path** is a peer mode, not a hardware-free test
  shim behind an env var.

---

## 4. IF-demodulator capability table

From the diagrams' reference table (same across IQ-NO-DRM and
IQ-DRM canvases). Bold rows are new relative to today's
`efd-proto::Mode` enum.

| Mode                        | Supported bandwidths            | Notes                |
|-----------------------------|---------------------------------|----------------------|
| CW-U, CW-L                  | 100 Hz, 150 Hz, 200 Hz, 500 Hz  |                      |
| **AM / SAM / SAM-U / SAM-L**| 4 kHz, 6 kHz, 10 kHz            | SAM variants new     |
| LSB / SSB / DSB             | 1.2 kHz, 2.5 kHz, 3 kHz         |                      |
| RTTY, PSK, CW, WEFAX        | 500 Hz, 1.5 kHz, 3 kHz          | digital data         |
| FreeDV, DRM                 | 3 kHz, 10 kHz                   | digital voice / data |

Note: "CW" shows up twice — once as an IF demodulator mode
(CW-U / CW-L, narrow passbands for listening / tuning), once
as a digital decoder (keying recovery from audio). Same letters,
different stages of the pipeline.

Current `efd-proto` has `AM, USB, LSB, CW, CWR, FM, DRM,
Unknown`. Rework adds `SAM`, `SAM-U`, `SAM-L` at minimum.

---

## 5. Client UI

From `client-sdr-UI.drawio`. Two layers:

- **UI-Structure**: grid with named cells, stable layout
  contract.
- **IQ-NO-DRM**: example fill for a live SDR-mode tuning session.

### 5.1 Named grid cells

```
┌──────────────────────────────────────────────────────────┐
│  display bar                                             │
│  ┌─────────┬──────────────────────────────┬──────────┐   │
│  │disp0-L  │        disp0-center          │ disp0-R  │   │
│  ├─────────┼──────────────────────────────┼──────────┤   │
│  │disp1-L  │        disp1-center          │ disp1-R  │   │
│  ├─────────┼──────────────────────────────┼──────────┤   │
│  │disp2-L  │        disp2-center          │ disp2-R  │   │
│  └─────────┴──────────────────────────────┴──────────┘   │
├──────────────────────────────────────────────────────────┤
│ a-axis-display │  spectrum                               │
├──────────────────────────────────────────────────────────┤
│                   f-axis-display                         │
├──────────────────────────────────────────────────────────┤
│ t-axis-display │  waterfall                              │
├──────────────────────────────────────────────────────────┤
│  control bar                                             │
│  ┌─────────┬──────────────────────────────┬──────────┐   │
│  │ctrl0-L  │        ctrl0-center          │ ctrl0-R  │   │
│  ├─────────┼──────────────────────────────┼──────────┤   │
│  │ctrl1-L  │        ctrl1-center          │ ctrl1-R  │   │
│  └─────────┴──────────────────────────────┴──────────┘   │
└──────────────────────────────────────────────────────────┘
```

Use these names as the stable IDs in CSS, layout trees, and
message routing. They survive mode changes; only cell *contents*
vary.

### 5.2 IQ-NO-DRM cell contents (example mode)

| Cell           | Contents                                                                 |
|----------------|--------------------------------------------------------------------------|
| `disp0-left`   | `AUD / IQ` — source class toggle                                         |
| `disp0-center` | Unified tuning line: `f 14 200 000 Hz · demod CWᵤ · bw 150 Hz · RIT +10 Hz · IF -15 Hz` |
| `disp0-right`  | `RX` badge + `-102 dBm` power meter                                      |
| `disp1-left`   | `FDM / HRF` — device selector within source class                        |
| `disp1-center` | `SNR 21 dB` + `DNR off / DNF off / APF off` + `decode CW`                |
| `disp1-right`  | S-meter bar (`S 9+20`) with live tick                                    |
| `disp2-left`   | Current source badge (e.g. `FDM IQ`)                                     |
| `disp2-center` | Live decoded text (`QST DE WA1W QST DE WA1W … 15 WPM TEST`)              |
| `disp2-right`  | `PASSTHROUGH` status                                                     |
| `spectrum`     | Vertical −10…−65 dBm scale; two dashed cursors mark filter passband      |
| `waterfall`    | 0…12 s time scale; two dashed cursors                                    |
| `ctrl0-left`   | `SRC`, `DEV` buttons                                                     |
| `ctrl0-center` | Click-to-tune fields: `f 14200000 Hz`, `bw 150 Hz`, `rit +10 Hz`, `IF +30 Hz` |
| `ctrl0-right`  | `WSJT-X` launcher                                                        |
| `ctrl1-left`   | `NB`, `APF` toggles (top), `DNR`, `DNF` toggles (bottom)                 |
| `ctrl1-center` | Top row: IF-demod mode buttons `AM / SAM / DSB / USB / LSB / CWᵤ / CWₗ / FMₙ`. Bottom row: decoder buttons `PSK / CW / MFSK / RTTY / FAX / PCKT / DRM / FDV` |
| `ctrl1-right`  | `REC` (top), `CONFIG` (bottom)                                           |

### 5.3 UI design principles captured by the diagram

- **IF demod and digital decoder are independent selectors.** The
  pipeline is two-stage; the UI exposes that.
- **Source selection and device selection are both first-class
  controls** — `AUD/IQ` + `FDM/HRF` in `disp0-left` / `disp1-left`,
  `SRC` / `DEV` buttons in `ctrl0-left`. The client drives runtime
  switching. Requires corresponding server-side support (see §2).
- **Live decoded text is inline** in `disp2-center`, not in a
  separate pop-up panel.
- **Chrome features**: `WSJT-X` launcher, `CONFIG` dialog, `REC`
  capture button.

### 5.4 New UI affordances relative to today

- Named grid with stable cell IDs.
- Inline unified tuning / state line.
- Inline decoded-text area.
- Explicit source-and-device selectors.
- `REC` button (see §6).

---

## 6. New features beyond topology

### 6.1 REC — recording to file

A single `REC` button in `ctrl1-right` captures either **IQ** or
**audio** to a file on disk, for later replay through the
file-source pipeline. The IQ analogue of FLAC-DRM (IQ-file
replay) is implied and should be a fifth pipeline topology.

Open questions (not yet decided):

- File format for IQ (raw `s16`, `complex f32`, SigMF?).
- File format for audio (WAV / FLAC already read by DREAM;
  same for both record and replay?).
- Naming convention (timestamped? user-labelled?).
- Where files live (Pi local disk? streamed to client for
  client-side storage?).

### 6.2 Runtime device model

Covered in §2. The server must expose:

- `EnumerateDevices` — list discovered audio-in, serial, IQ-USB
  devices, each tagged with capability flags from `efd-proto`.
- `SelectSource` — switch source class (audio ↔ IQ).
- `SelectDevice` — pick a device within the active source class.
- `SaveState` / `LoadState` — triggered automatically at
  quit / start; also exposed as client commands for explicit
  snapshot / restore.

### 6.3 WSJT-X integration

The UI has a `WSJT-X` quick-launch button. Open question: does
the client launch WSJT-X locally (with a pre-configured
rigctld tunnel to the Pi's `127.0.0.1:4532`), or does the
server launch it on the Pi, or does the button simply open
a config dialog and the user does the tunnel themselves?
Not yet decided.

---

## 7. Summary of architectural shifts vs current code

| Area                | Current (0.6.14)                              | Rework                                                   |
|---------------------|-----------------------------------------------|----------------------------------------------------------|
| Source selection    | Config-pinned at startup                      | Discovered at startup, swappable at runtime              |
| State persistence   | None — config only                            | Saved state on quit, validated on start                  |
| Pipeline structure  | Linear per mode, monolithic                   | Four (five with IQ-replay) named topologies              |
| NB placement        | Audio-domain                                  | Pre-IF, on raw IQ                                        |
| DSP block           | Ad-hoc (AGC, DC block, filters)               | Named four: DNB / DNR / DNF / APF                        |
| DRM IF feed         | LSB + USB (CLAUDE.md phrasing)                | Single USB, −5 kHz offset, 10 kHz BW                     |
| DRM file path       | Hardware-free test behind `EFD_DRM_FILE_TEST` | First-class FLAC-DRM mode                                |
| Mode set            | `AM, USB, LSB, CW, CWR, FM, DRM, Unknown`     | adds `SAM`, `SAM-U`, `SAM-L`                             |
| Decoder set         | `cw, rtty, psk, wspr, ft8, aprs, wefax`       | adds `PCKT`, `MFSK`; `FAX`, `PSK`, `CW` confirmed        |
| Recording           | No                                            | REC button; IQ-or-audio → file; file → replay pipeline    |
| Client UI           | Ad-hoc controls pane                          | Named grid, inline state + decoded text, live selectors  |
| CAT (hardware)      | Direct USB native CAT                         | Unchanged (FDM-DUO only)                                 |
| CAT (external apps) | hand-rolled rigctld responder                 | Unchanged; WSJT-X launcher in UI                         |

---

## 8. Confirmed design points

User-confirmed refinements of CLAUDE.md (see memory
`project_rework_design_notes`):

1. **IQ-DRM** uses a single USB demod at −5 kHz offset, 10 kHz BW.
2. **`dream -f`** is reserved for the FLAC-DRM replay path.
3. **REC** captures IQ *or* audio to file, to be replayed via the
   file-source pipeline.
4. **NB pre-IF** — move the noise blanker upstream of the IF
   demod.
5. **UI grid names** (§5.1) are the stable cell identifiers.

---

## 9. Suggested sequencing

Broad order of attack; each phase can be its own PR and version
bump. This is a proposal, not a commitment.

1. **`efd-proto` rewrite.** New `Mode` (adds `SAM-U`, `SAM-L`),
   new decoder set (adds `PCKT`, `MFSK`), new
   `EnumerateDevices` / `SelectSource` / `SelectDevice` /
   `SaveState` / `LoadState` / `Rec` message types, grid-cell
   identifiers as a shared enum, `RadioState` cleaned up to
   carry the unified tuning line's fields, `filter_bw_hz`
   alongside the existing `filter_bw: String` (see review
   batch #16). Breaks wire; server and client deploy together.
2. **Backend device model.** Discovery + saved state + runtime
   source / device swap. Pipeline gains a
   `Pipeline::reconfigure(new_source, new_device)` path. The
   existing `config.toml` demotes to defaults-only.
3. **Pipeline topology.** Move NB pre-IF. Split DSP into named
   DNB / DNR / DNF / APF blocks. Single-USB DRM IF feed. First-
   class FLAC-DRM and (new) IQ-file replay paths. The four
   existing pipeline diagrams map 1:1 to code paths.
4. **REC feature.** File format decisions (§6.1). Server-side
   recorder task that taps the same broadcast channels the WS
   downstream uses. Client button + file-name UI.
5. **Client UI rewrite.** Named grid implementation in GTK. Cell
   contents for all five modes (AUD, IQ-NO-DRM, IQ-DRM, FLAC-DRM,
   IQ-replay). Live decoded-text area. Source/device selectors.
   `WSJT-X`, `REC`, `CONFIG` chrome.

Phase 1 unblocks everything else. Phase 5 is the largest but
pure client work, can be done in parallel with 2 / 3 / 4 once
the proto lands.

---

## 10. Open questions

Not design refusals — just things we haven't pinned down yet:

- **REC file formats** for IQ and audio (§6.1).
- **WSJT-X launcher behaviour** — client-local or Pi-hosted
  (§6.3).
- **IQ-file replay pipeline** — presumably mirrors FLAC-DRM, but
  which decoder routes does it drive? IF demod → DSP + digital
  decode? Needs a dedicated diagram.
- **Saved-state schema** — TOML on the Pi alongside config? Or
  a separate dotfile? Plain serde or versioned?
- **Device enumeration cadence** — once at startup, or
  re-enumerate when a USB hot-plug event fires? The flowchart
  says the former; the latter is more forgiving.
- **Capability flags** for each discovered device (has TX?
  has hardware CAT? supported sample rates? supported modes?) —
  needs to align with the existing `efd-proto::Capabilities`.
