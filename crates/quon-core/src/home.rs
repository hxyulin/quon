//! Quon's per-user home directory, `$QUON_HOME` or `~/.quon`.
//!
//! The home holds state that is shared by every project on the machine
//! and does not belong in one project's `build/` directory. Today that
//! is the compiled quon-config library that `quon.rs` links against
//! (see [`crate::script_runner::rlib`]).

use crate::error::{Error, Result};
use std::path::PathBuf;

/// The environment variable that overrides the home directory.
pub const QUON_HOME_ENV: &str = "QUON_HOME";

/// `$QUON_HOME` if set and non-empty, else `~/.quon`. A relative
/// `$QUON_HOME` is made absolute against the current directory,
/// because the paths under it are handed to cargo, which runs in a
/// different directory.
pub fn quon_home() -> Result<PathBuf> {
    let home = match std::env::var_os(QUON_HOME_ENV).filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => std::env::home_dir()
            .ok_or_else(|| {
                Error::Config(format!(
                    "cannot find the home directory for ~/.quon; set {QUON_HOME_ENV}"
                ))
            })?
            .join(".quon"),
    };
    std::path::absolute(&home).map_err(|e| Error::Io {
        path: home,
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_never_use_the_real_home() {
        // `.cargo/config.toml` points QUON_HOME at the checkout's
        // `target/` for every process cargo starts. If a test runner
        // stops honouring it, tests would write into `~/.quon`.
        let home = quon_home().unwrap();
        assert!(
            std::env::var_os(QUON_HOME_ENV).is_some(),
            "QUON_HOME is not set for tests; check .cargo/config.toml"
        );
        assert_ne!(Some(home), std::env::home_dir().map(|h| h.join(".quon")));
    }
}
