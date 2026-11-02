// AppKit must run on the real main thread, not a libtest worker thread.
#[cfg(target_os = "macos")]
#[path = "../src/types.rs"]
mod types;
#[cfg(target_os = "macos")]
pub use types::{Choice, Event, Outcome, Request};
#[cfg(target_os = "macos")]
#[path = "../src/macos.rs"]
mod macos;

fn main() {
    #[cfg(target_os = "macos")]
    macos::verify_native_controls();
}
