//! Platform presentation adapters. Authorization, deadlines and remembered
//! choices stay in `consent` / `server`, independently of the window toolkit.
#[cfg(all(feature = "desktop", any(windows, target_os = "macos")))]
#[path = "desktop/native.rs"]
mod native;
#[cfg(all(target_os = "linux", feature = "desktop"))]
mod webview;

#[cfg(all(feature = "desktop", any(windows, target_os = "macos")))]
pub use native::{Desktop, available};
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub use webview::{Desktop, available};

#[cfg(not(feature = "desktop"))]
pub fn available() -> bool {
    false
}
