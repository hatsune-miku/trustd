//! Native presentation only: no daemon connection, token, shell or authorization
//! policy.
//! The caller supplies an in-process one-shot callback for desktop prompts.
//!
//! All FFI lives in this small crate so the application can retain its
//! `forbid(unsafe_code)` boundary. Platform windows and callback pointers never leave
//! the owning UI thread; the cross-thread sender transports owned Rust data.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(any(windows, target_os = "macos"))]
mod types;
#[cfg(any(windows, target_os = "macos"))]
pub use types::{Choice, Event, Outcome, Request};
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{EventLoop, Sender};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{EventLoop, Sender};
