//! Project-root discovery.
//!
//! Walks upward from a starting directory looking for a `quon.rs`
//! file and returns its enclosing directory. Lives in
//! `quon-core` (not `quon`) so that embedders — editor plugins,
//! LSPs, future `quon-*` plugin binaries — can reuse the same discovery
//! logic without re-implementing it.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Walk upward from `start` looking for a `quon.rs` file. Returns the
/// directory that contains it.
///
/// `start` may be either a file or a directory. If it is a file, the
/// walk begins from its parent. The search terminates when the marker is
/// found or the filesystem root is reached without finding one.
///
/// ### Why not record the script path itself?
///
/// Callers (CLI, LSP, embedders) want to anchor relative paths against
/// the *project root*, not against the script. Returning the directory
/// keeps the project-root concept opaque to consumers — they don't have
/// to know whether the marker file is `quon.rs`, `quon.toml`, or
/// `.quonrc` in some future world.
pub fn find_project_root(start: &Path) -> Result<PathBuf> {
    let mut cursor: PathBuf = if start.is_file() {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"))
    } else {
        start.to_path_buf()
    };

    loop {
        if cursor.join("quon.rs").is_file() {
            return Ok(cursor);
        }
        if !cursor.pop() {
            return Err(Error::Config(format!(
                "no quon.rs found in {} or any parent directory",
                start.display()
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn errors_when_no_marker_anywhere() {
        // Use a child dir under tempdir so we definitely won't accidentally
        // find a real quon.rs in the user's filesystem ancestors.
        let tmp = tempfile::tempdir().expect("tempdir");
        let child = tmp.path().join("only-this");
        fs::create_dir_all(&child).expect("mkdir child");

        // Note: this test will spuriously pass if a quon.rs
        // exists in /tmp or any ancestor — but in practice tempdir's prefix is
        // always under a controlled location, and CI environments do not
        // have spurious marker files at /tmp/.
        let result = find_project_root(&child);
        match result {
            Err(Error::Config(msg)) => {
                assert!(
                    msg.contains("quon.rs"),
                    "error message should mention quon.rs, got: {msg}"
                );
            }
            Ok(p) => panic!("unexpected success at {p:?}"),
            Err(other) => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[test]
    fn finds_quon_rs_from_each_start() {
        type MakeStart = fn(&Path) -> PathBuf;
        let cases: [(&str, MakeStart); 3] = [
            ("starting directory", |root| root.to_path_buf()),
            ("nested subdirectory", |root| {
                let nested = root.join("a").join("b").join("c");
                fs::create_dir_all(&nested).expect("mkdir nested");
                nested
            }),
            ("file path as start", |root| {
                let nested = root.join("a");
                fs::create_dir_all(&nested).expect("mkdir nested");
                let some_file = nested.join("foo.rs");
                fs::write(&some_file, b"fn main(){}").expect("write file");
                some_file
            }),
        ];
        for (name, make_start) in cases {
            let tmp = tempfile::tempdir().expect("tempdir");
            fs::write(tmp.path().join("quon.rs"), b"// stub").expect("write script");
            let start = make_start(tmp.path());

            let got = find_project_root(&start).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(
                got.canonicalize().unwrap(),
                tmp.path().canonicalize().unwrap(),
                "{name}"
            );
        }
    }
}
