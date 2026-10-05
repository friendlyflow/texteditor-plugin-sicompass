//! The OS trash, through the app (`sicompass_sdk::plugin::desktop`), so a
//! delete and its undo are the app's like every other program's.
//!
//! A trait so the tests can swap it: their fake removes the item outright and
//! keeps nothing, because a test must never put a fixture in the developer's
//! real trash (about a thousand runs once left 37 850 of them there). Outside
//! sicompass, [`HostDesktop`] has no app to ask, and refuses.

use std::path::{Path, PathBuf};

pub trait Desktop {
    /// Move `path` to the OS trash, so the user can get it back.
    fn trash(&self, path: &Path) -> Result<(), String>;
    /// Restore the most recently trashed item that was at `path`.
    fn restore(&self, path: &Path) -> Result<(), String>;
    /// The target of the symlink at `path`.
    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }

    /// `path` through every symlink along it (`sicompass_sdk::fs_links`).
    fn resolve(&self, path: &Path) -> PathBuf {
        sicompass_sdk::fs_links::resolve_with(path, |p| self.read_link(p))
    }
}

/// The app's `desktop` services.
pub struct HostDesktop;

impl Desktop for HostDesktop {
    fn trash(&self, path: &Path) -> Result<(), String> {
        sicompass_sdk::plugin::desktop::trash(&path.to_string_lossy())
    }

    fn restore(&self, path: &Path) -> Result<(), String> {
        sicompass_sdk::plugin::desktop::restore(&path.to_string_lossy())
    }
}
