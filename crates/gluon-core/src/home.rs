//! Gluon's per-user home directory, `$GLUON_HOME` or `~/.gluon`.
//!
//! The home holds state that is shared by every project on the machine
//! and does not belong in one project's `build/` directory. Today that
//! is the compiled gluon-config library that `gluon.rs` links against
//! (see [`crate::script_runner::rlib`]).

use crate::error::{Error, Result};
use std::path::PathBuf;

/// The environment variable that overrides the home directory.
pub const GLUON_HOME_ENV: &str = "GLUON_HOME";

/// `$GLUON_HOME` if set and non-empty, else `~/.gluon`. A relative
/// `$GLUON_HOME` is made absolute against the current directory,
/// because the paths under it are handed to cargo, which runs in a
/// different directory.
pub fn gluon_home() -> Result<PathBuf> {
    let home = match std::env::var_os(GLUON_HOME_ENV).filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => std::env::home_dir()
            .ok_or_else(|| {
                Error::Config(format!(
                    "cannot find the home directory for ~/.gluon; set {GLUON_HOME_ENV}"
                ))
            })?
            .join(".gluon"),
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
        // `.cargo/config.toml` points GLUON_HOME at the checkout's
        // `target/` for every process cargo starts. If a test runner
        // stops honouring it, tests would write into `~/.gluon`.
        let home = gluon_home().unwrap();
        assert!(
            std::env::var_os(GLUON_HOME_ENV).is_some(),
            "GLUON_HOME is not set for tests; check .cargo/config.toml"
        );
        assert_ne!(Some(home), std::env::home_dir().map(|h| h.join(".gluon")));
    }
}
