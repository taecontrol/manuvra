//! Low-level Chromium DevTools Protocol building blocks for Manuvra.

pub mod endpoint;
pub mod launch;
pub mod page;
pub mod transport;

pub use endpoint::{Endpoint, EndpointError};
pub use launch::{GOOGLE_CHROME_MACOS, LaunchError, LaunchRequest, launch_dedicated_chrome};
pub use page::{PageError, Screenshot};
pub use transport::{CdpClient, CommandFailure, CommandOutcome, JournalEvent, JournalSnapshot};
