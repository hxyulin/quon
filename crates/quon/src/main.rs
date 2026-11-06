//! The `quon` binary: the library's command line, run from the current
//! directory.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::current_dir() {
        Ok(dir) => quon::run(&dir, std::env::args_os().skip(1)),
        Err(e) => {
            eprintln!("error: failed to read the current directory: {e}");
            ExitCode::FAILURE
        }
    }
}
