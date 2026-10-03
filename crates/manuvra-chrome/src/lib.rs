//! Low-level Chromium DevTools Protocol building blocks for Manuvra.

mod endpoint;
mod input;
mod observation;
mod owned;
mod page;
mod transport;

pub use input::{
    InputCancellation, Key, PerformError, PerformFact, PreparedInput, PreparedOperation,
};
pub use observation::{
    ActiveDescendant, ColorChannel, ColorObservation, ColorScope, ColorScopeKind, ComputedColor,
    Coverage, Element, FocusAnchor, FocusSurface, HoverRegion, Observation, Overlay,
    PaintedTextObservation, Rect, ScrollPosition, ScrollRegion, ScrollTarget, SelectOption,
    TextInventory, ViewportState,
};
pub use owned::{
    BrowserConfig, BrowserError, BrowserProvenance, CapturedPage, OwnedBrowser, ProvenanceViewport,
    RedactionProof,
};
pub use page::Screenshot;
