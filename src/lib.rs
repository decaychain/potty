//! Library surface shared between the `potty` binary and the `potty-notify` helper.
//!
//! The attention-feed wire contract, remote protocol, SSH client, and small cross-binary helpers
//! live here so `potty`, `potty-session`, and `potty-notify` can share them without drift.

pub mod notify;
pub mod proto;
pub mod remote;
pub mod term_env;
