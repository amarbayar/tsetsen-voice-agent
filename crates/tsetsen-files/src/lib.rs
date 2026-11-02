//! Portable file services for Tsetsen.
//!
//! [`WalkFiles`] finds the user's own documents by walking a few folders,
//! and works on every OS. Where the OS has an index (Spotlight on macOS,
//! Windows Search), its adapter is faster and also matches text inside
//! files; this is the fallback. [`SystemLauncher`] opens a file the way
//! double-clicking would, with the OS's own opener.
//!
//! Only the folders given are searched, never system or app folders, and
//! nothing is read beyond names, sizes and dates.

mod launcher;
mod walk;

pub use launcher::SystemLauncher;
pub use walk::{WalkFiles, user_folders};
