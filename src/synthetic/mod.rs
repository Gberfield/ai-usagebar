//! Synthetic subscription quota from the documented `/v2/quotas` API.

pub mod fetch;
pub mod types;
pub mod vendor;

pub use fetch::{FetchOutcome, fetch_snapshot};
