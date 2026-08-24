//! Platform-independent core: the domain model, the ports the application layer
//! depends on, and the sync loop and supervisor that drive them.
//!
//! Nothing here may depend on `zync-adapters`. That direction is enforced by the
//! crate graph rather than by review.

pub mod app;
pub mod domain;
pub mod ports;
