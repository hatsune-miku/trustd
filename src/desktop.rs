//! Platform presentation adapters. Authorization, deadlines and remembered
//! choices stay in `consent` / `server`, independently of the window toolkit.
#[cfg(any(windows, target_os = "macos"))]
#[path = "desktop/native.rs"]
mod native;
#[cfg(target_os = "linux")]
mod webview;

#[cfg(any(windows, target_os = "macos"))]
pub use native::{Desktop, available};
#[cfg(target_os = "linux")]
pub use webview::{Desktop, available};
