//! Errors of the QEMU runner. Quon prints them as they are, so each
//! message keeps the wording and prefix it had when this code lived in
//! `quon-core`.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("compile error: {0}")]
    Compile(String),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("no OVMF firmware found.\n{attempts}")]
    OvmfNotFound { attempts: String },

    #[error("ESP source path does not exist: {path}")]
    EspMissing { path: PathBuf },

    #[error(
        "cannot pick a default QEMU binary for target '{triple}': unknown architecture.\nSet one explicitly with `qemu(\"qemu-system-<arch>\")` in your quon.rs."
    )]
    UnknownQemuTarget { triple: String },
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for quon_runner::RunnerError {
    fn from(e: Error) -> Self {
        quon_runner::RunnerError(e.to_string())
    }
}
