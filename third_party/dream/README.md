# Vendored DREAM 2.1.1 DRM decoder

## Why vendored

- The in-distro `dream-drm` 2.2 has documented decoding regressions (see
  [openwebrx wiki notes](https://github.com/jketterl/openwebrx/wiki/DRM-demodulator-notes)).
  2.1.1 is the last known-good release for DRM reception.
- The stock 2.2 package is built with GUI enabled. We need
  `qmake CONFIG+=console` to produce a headless binary suitable for the
  CM5 backend.
- DREAM 2.1.1's `Hamlib.cpp` doesn't compile against modern hamlib
  (`rig_model_t` no longer disambiguates between `int` and `_REAL` in
  `CSettings::Put()`). The patch here is a two-line cast fix.

## Layout

- `dream-2.1.1-svn808.tar.gz` — upstream tarball from SourceForge, unchanged.
- `0001-hamlib-cast-rig_model_t-to-int.patch` — the minimum build fix.
- `build.sh` — extract + patch + `qmake CONFIG+=console` + `make`. Produces `build/install/bin/dream`.
- `samples/` — known-good DRM recordings used by `efd-dsp::drm` integration tests. Mono 16-bit FLAC IF recordings of real broadcasts.

## Building

```
./third_party/dream/build.sh
```

Installs the binary to `third_party/dream/build/install/bin/dream`. The
PKGBUILD for the `efd-station` package runs this at package build time
and installs to `/usr/bin/dream` alongside the server binary.

## Runtime wiring

The `efd-dsp::drm` module creates two PipeWire null sinks on startup —
`drm_in` and `drm_out` — and invokes dream as:

```
dream -I drm_in.monitor -O drm_out -c 6 --sigsrate 48000 --audsrate 48000
```

`-c 6` is "I/Q input positive, 0 Hz IF" — appropriate for baseband IQ
from the SDR pipeline (the IQ is already tuned to the DRM channel). The
Rust side writes 48 kHz interleaved int16 stereo (L=I, R=Q) into the
`drm_in` sink, reads decoded stereo PCM from `drm_out.monitor`, and
publishes it to the shared `broadcast<AudioSamples>`.

## License

DREAM is GPL-2.0+. The vendored tarball and the patch inherit that
license. The rest of efd-station may link/depend on it as a separate
process (subprocess execution, not a library link), which is not a
license-mixing concern in practice.
