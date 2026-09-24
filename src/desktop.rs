//! The OS trash, through the host.
//!
//! A trait so the tests run natively: there the fake removes the item outright
//! and keeps nothing, because a test must never put a fixture in the
//! developer's real trash (about a thousand runs once left 37 850 of them
//! there). Inside the sandbox it is the host's `desktop` interface, which only
//! reaches paths this plugin was granted.

use std::path::{Path, PathBuf};

pub trait Desktop {
    /// Move `path` to the OS trash, so the user can get it back.
    fn trash(&self, path: &Path) -> Result<(), String>;
    /// Restore the most recently trashed item that was at `path`.
    fn restore(&self, path: &Path) -> Result<(), String>;
    /// The target of the symlink at `path`. The sandbox never reads an
    /// absolute one itself, so this asks the host.
    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }

    /// `path` through every symlink along it (`sicompass_sdk::fs_links`).
    fn resolve(&self, path: &Path) -> PathBuf {
        sicompass_sdk::fs_links::resolve_with(path, |p| self.read_link(p))
    }
}

/// The host's `desktop` interface.
pub struct HostDesktop;

#[cfg(target_arch = "wasm32")]
impl Desktop for HostDesktop {
    fn trash(&self, path: &Path) -> Result<(), String> {
        sicompass_pdk::desktop::trash(&path.to_string_lossy())
    }

    fn restore(&self, path: &Path) -> Result<(), String> {
        sicompass_pdk::desktop::restore(&path.to_string_lossy())
    }

    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        sicompass_pdk::desktop::read_link(&path.to_string_lossy())
            .ok()
            .map(PathBuf::from)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Desktop for HostDesktop {
    fn trash(&self, _path: &Path) -> Result<(), String> {
        Err("no OS trash outside the sandbox".to_owned())
    }

    fn restore(&self, _path: &Path) -> Result<(), String> {
        Err("no OS trash outside the sandbox".to_owned())
    }
}
