//! Low-level Chromium DevTools Protocol building blocks for Manuvra.

pub mod endpoint;
pub mod observation;
pub mod owned;
pub mod page;
pub mod transport;

pub use endpoint::{Endpoint, EndpointError};
pub use observation::{Coverage, Element, Observation, Rect, ViewportState};
pub use owned::{
    BrowserConfig, BrowserError, BrowserProvenance, CapturedPage, OwnedBrowser, RedactionProof,
};
pub use page::{PageError, Screenshot};
pub use transport::{CdpClient, CommandFailure, CommandOutcome, JournalEvent, JournalSnapshot};
