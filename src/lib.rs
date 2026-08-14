//! Persistent terminal sessions you can leave and come back to.
//!
//! The node owns the PTY, so disconnecting a client never touches the child
//! process. Other machines are reached by running `mm agent` there over ssh,
//! which means your existing ssh setup decides both how to get there and who is
//! allowed in: manymux keeps no keys and no allowlist of its own.
//!
//! Building without the `desktop` feature drops the parts that need a real
//! terminal or a PTY, leaving the client core that a mobile app links against.

pub mod client;
pub mod config;
pub mod hosts;
pub mod proto;
pub mod ssh;
pub mod style;
pub mod term;
pub mod user;

#[cfg(feature = "desktop")]
pub mod clipboard;
#[cfg(feature = "desktop")]
pub mod ipc;
#[cfg(feature = "desktop")]
pub mod log;
#[cfg(feature = "desktop")]
pub mod node;
#[cfg(feature = "desktop")]
pub mod service;
#[cfg(feature = "desktop")]
pub mod update;
