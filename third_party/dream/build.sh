#!/usr/bin/env bash
# Build the DREAM 2.1.1 console decoder from the vendored tarball.
#
# Usage: ./build.sh [--prefix=DIR]
#   --prefix=DIR  install prefix (default: ./build/install)
#
# Produces a `dream` binary that:
#   - has no Qt dependency (CONFIG+=console strips Qt entirely)
#   - links against libpulse for sound I/O — PipeWire on CM5 Trixie
#     implements the PulseAudio protocol, so the same binary works on
#     both dev machines and the CM5
#   - includes a hamlib cast patch needed to compile against any
#     hamlib ≥ ~4.2 where rig_model_t is no longer implicitly int
#
# Required system packages (Arch/Manjaro): qt5-base, fftw, zlib,
# libsndfile, libpulse, speexdsp, hamlib, libpcap, opus, libsamplerate.
# On Debian/Trixie: qtbase5-dev, qt5-qmake, libfftw3-dev, zlib1g-dev,
# libsndfile1-dev, libpulse-dev, libspeexdsp-dev, libhamlib-dev,
# libpcap-dev, libopus-dev, libsamplerate0-dev.

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
TARBALL="$HERE/dream-2.1.1-svn808.tar.gz"
PATCH="$HERE/0001-hamlib-cast-rig_model_t-to-int.patch"
BUILD_DIR="$HERE/build"
PREFIX="$BUILD_DIR/install"

for arg in "$@"; do
    case "$arg" in
        --prefix=*) PREFIX="${arg#--prefix=}" ;;
        *) echo "unknown arg: $arg" >&2; exit 1 ;;
    esac
done

[[ -f "$TARBALL" ]] || { echo "missing $TARBALL" >&2; exit 1; }
[[ -f "$PATCH" ]]   || { echo "missing $PATCH" >&2; exit 1; }

mkdir -p "$BUILD_DIR"
cd "$BUILD_DIR"

# Fresh extraction every time — this script is idempotent.
rm -rf dream
tar -xzf "$TARBALL"

cd dream
patch -p2 < "$PATCH"
qmake CONFIG+=console
make -j"$(nproc)"

mkdir -p "$PREFIX/bin"
install -m755 dream "$PREFIX/bin/dream"

echo
echo "Built: $PREFIX/bin/dream"
"$PREFIX/bin/dream" --help 2>&1 | head -1 || true
