//! Stable identifiers for the client's layout grid cells.
//!
//! The client UI is built on a named grid (see
//! `docs/client-sdr-UI.drawio`). The cell names are shared between
//! server and client so either side can address a cell — for example
//! to emit a per-cell error, route a "flash" indication on state
//! change, or drive future per-mode layout data from the server.
//!
//! The names are structural and survive mode changes; only the *fill*
//! of each cell varies per mode.

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// Named layout cell in the GTK client. Order matches the visual
/// layout top-to-bottom, left-to-right; reorderings are a wire break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum GridCell {
    Disp0Left,
    Disp0Center,
    Disp0Right,
    Disp1Left,
    Disp1Center,
    Disp1Right,
    Disp2Left,
    Disp2Center,
    Disp2Right,

    Spectrum,
    /// Vertical amplitude axis (dBm scale) alongside `Spectrum`.
    AmpAxis,
    /// Horizontal frequency axis between `Spectrum` and `Waterfall`.
    FreqAxis,
    Waterfall,
    /// Vertical time axis alongside `Waterfall`.
    TimeAxis,

    Ctrl0Left,
    Ctrl0Center,
    Ctrl0Right,
    Ctrl1Left,
    Ctrl1Center,
    Ctrl1Right,
}
