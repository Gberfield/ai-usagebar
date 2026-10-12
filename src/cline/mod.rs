//! Cline (ClinePass) subscription quota integration.
//!
//! Cline publishes the caps on the plan and the consumption as a ledger, and
//! nothing joins the two — so this vendor does. See [`types`] for the window
//! arithmetic and the reason no figure here is printed in dollars.

pub mod creds;
pub mod fetch;
pub mod types;
pub mod vendor;
