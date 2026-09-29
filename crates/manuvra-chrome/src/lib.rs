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
    ActiveDescendant, Coverage, Element, FocusAnchor, FocusSurface, HoverRegion, Observation, Rect,
    SelectOption, ViewportState,
};
pub use owned::{
    BrowserConfig, BrowserError, BrowserProvenance, CapturedPage, OwnedBrowser, ProvenanceViewport,
    RedactionProof,
};
pub use page::Screenshot;
