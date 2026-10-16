//! Synthetic (`api.synthetic.new`) vendor — subscription quotas from
//! `/v2/quotas` over an API key.
//!
//! The endpoint answers with a flat set of independent allowances (5h
//! requests, weekly credits, hourly search, free tool calls) rather than one
//! nested plan object, and the documented example only shows `subscription`.
//! The parser therefore treats every allowance except the two that make up a
//! subscription as optional — see `types::Quotas`.

pub mod fetch;
pub mod types;
pub mod vendor;

pub use fetch::{FetchOutcome, fetch_snapshot};
