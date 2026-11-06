//! Doctests of host-test packages: the `rustdoc --test` command, and
//! the file that carries it from the build to `quon test`.
//!
//! rustdoc compiles the lib's source to find its doctests, expanding
//! macros as it goes, so it needs what the lib's own compile gets:
//! feature cfgs, the build script's cfgs, `OUT_DIR` and `rustc-env`
//! variables, lint levels, and every dependency's `--extern`. The
//! doctests themselves also link the dev-dependencies, as in Cargo. The
//! lib test unit (the lib built with `--test`) gets exactly that set,
//! so the doctest command is derived from its rustc command: the
//! output flags go, and an `--extern` for the package's plain lib is
//! added, which is what the doctests `use`.
//!
//! The command is known only once the unit's build-script run has
//! finished, so the compile step writes it to a file next to the test
//! binary, and `quon test` reads it from there (see
//! [`crate::run::host`]).
//!
//! Freshness: the compile step writes the file every time it considers
//! the lib test unit, whether rustc runs or the cache serves it, and
//! only after the unit succeeds. The command comes from the same argv
//! the unit's cache key hashes, so the file always matches the binary
//! beside it.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A `rustdoc --test` invocation, without the libtest arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctestCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Sorted by name. A list, because JSON object keys must be
    /// strings and these are `OsString`s.
    pub env: Vec<(OsString, OsString)>,
}

/// Where the doctest command of the lib test at `binary` is stored.
pub fn command_file(binary: &Path) -> PathBuf {
    let mut name = binary.as_os_str().to_os_string();
    name.push(".doctest.json");
    PathBuf::from(name)
}

/// Read the doctest command stored next to `binary`.
pub fn read_command(binary: &Path) -> Result<DoctestCommand> {
    let path = command_file(binary);
    let text = std::fs::read(&path).map_err(|e| Error::Io {
        path: path.clone(),
        source: e,
    })?;
    serde_json::from_slice(&text)
        .map_err(|e| Error::Config(format!("{}: invalid doctest command: {e}", path.display())))
}

pub(crate) fn write_command(binary: &Path, cmd: &DoctestCommand) -> Result<()> {
    let path = command_file(binary);
    let json = serde_json::to_vec_pretty(cmd)
        .map_err(|e| Error::Compile(format!("serialize the doctest command: {e}")))?;
    std::fs::write(&path, json).map_err(|e| Error::Io { path, source: e })
}

/// The `rustdoc --test` arguments for a lib whose lib test unit was
/// compiled with `rustc_args`. `lib` is the lib's crate name and
/// `lib_path` its plain build.
///
/// `--test` stays: rustdoc reads it as "run the doctests". What goes is
/// what names rustc's own outputs, which rustdoc does not write:
/// `--out-dir`, `-o`, `--emit`, the incremental directory, and the
/// metadata hash and file-name suffix.
pub(crate) fn rustdoc_args(rustc_args: &[OsString], lib: &str, lib_path: &Path) -> Vec<OsString> {
    let mut out = Vec::with_capacity(rustc_args.len() + 2);
    let mut args = rustc_args.iter();
    while let Some(arg) = args.next() {
        let s = arg.to_string_lossy();
        match s.as_ref() {
            "--out-dir" | "-o" => {
                args.next();
            }
            "-C" => {
                let Some(value) = args.next() else { break };
                if !output_codegen_option(&value.to_string_lossy()) {
                    out.push(arg.clone());
                    out.push(value.clone());
                }
            }
            s if s.starts_with("--emit=") => {}
            s if s.starts_with("-C") && output_codegen_option(&s[2..]) => {}
            _ => out.push(arg.clone()),
        }
    }
    let mut ext = OsString::from(lib);
    ext.push("=");
    ext.push(lib_path.as_os_str());
    out.push("--extern".into());
    out.push(ext);
    out
}

fn output_codegen_option(value: &str) -> bool {
    ["incremental=", "metadata=", "extra-filename="]
        .iter()
        .any(|p| value.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn rustdoc_args_drop_rustc_outputs_and_link_the_lib() {
        let rustc = os(&[
            "/p/calc/src/lib.rs",
            "--crate-name",
            "calc",
            "--test",
            "--edition=2021",
            "--out-dir",
            "/b/deps",
            "--emit=dep-info=/b/deps/calc.d,link",
            "--extern",
            "helper=/b/deps/libhelper-1.rlib",
            "-L",
            "dependency=/b/deps",
            "-C",
            "opt-level=0",
            "--cfg",
            "feature=\"std\"",
            "-C",
            "incremental=/b/incr",
            "-Cmetadata=abc",
            "-C",
            "extra-filename=-abc",
        ]);
        assert_eq!(
            rustdoc_args(&rustc, "calc", Path::new("/b/deps/libcalc-2.rlib")),
            os(&[
                "/p/calc/src/lib.rs",
                "--crate-name",
                "calc",
                "--test",
                "--edition=2021",
                "--extern",
                "helper=/b/deps/libhelper-1.rlib",
                "-L",
                "dependency=/b/deps",
                "-C",
                "opt-level=0",
                "--cfg",
                "feature=\"std\"",
                "--extern",
                "calc=/b/deps/libcalc-2.rlib",
            ])
        );
    }

    #[test]
    fn the_command_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("calc-abc");
        let cmd = DoctestCommand {
            program: "/t/bin/rustdoc".into(),
            args: os(&["--test", "src/lib.rs"]),
            env: vec![("OUT_DIR".into(), "/b/out".into())],
        };
        write_command(&binary, &cmd).unwrap();
        assert_eq!(
            command_file(&binary),
            dir.path().join("calc-abc.doctest.json")
        );
        assert_eq!(read_command(&binary).unwrap(), cmd);
    }
}
