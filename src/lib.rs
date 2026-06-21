//! Library surface shared between the `potty` binary and the `potty-notify` helper.
//!
//! Only the attention-feed wire contract lives here — everything else is private to the binary
//! (`src/main.rs` and its `mod`s). Keeping the contract in one place means the listener and the
//! sender can never drift out of sync.

pub mod notify;
pub mod proto;
