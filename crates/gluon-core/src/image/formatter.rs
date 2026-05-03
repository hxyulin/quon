//! `PartitionFormatter` — dispatch surface for per-partition
//! filesystem formatting. The orchestrator (`build_partitioned_disk`)
//! resolves partition layout; this trait fills each partition's
//! byte range with a valid filesystem of the requested kind.
//!
//! Today there is one shipped implementation: `Fat32Formatter` in
//! `image/fat.rs`. Adding a new format means: (1) implement the
//! trait, (2) add an arm to `select_formatter`. No edits to
//! `format_partition_in_place` or `build_partitioned_disk`.
//!
//! See the Stage 7 plan's "Decisions baked in" §1-§4 for why this
//! is a static-dispatch function rather than a dynamic registry,
//! and why the test stub is not optional.

use crate::error::{Error, Result};
use crate::image::ResolvedEntry;
use std::path::Path;

/// Format a partition's byte range with a filesystem.
pub trait PartitionFormatter: Send + Sync {
    /// Write a filesystem into the byte range
    /// `[start_byte .. start_byte + len_bytes)` of `image_path`.
    ///
    /// `scoped_name` is a human-readable label for the partition
    /// (typically `"<image>::partition(<name>)"`); it is used in
    /// error messages and progress output, never in on-disk
    /// metadata.
    ///
    /// `entries` are the already-resolved file/directory contents
    /// destined for the filesystem.
    ///
    /// `stdout` is a progress sink; impls append human-readable
    /// per-entry progress lines (consistent with the existing
    /// `format_fat32_into` behaviour).
    fn format_in_place(
        &self,
        image_path: &Path,
        start_byte: u64,
        len_bytes: u64,
        scoped_name: &str,
        entries: &[ResolvedEntry],
        stdout: &mut Vec<u8>,
    ) -> Result<()>;
}

/// Resolve `fs_type` to the formatter that handles it.
///
/// The error message must be identical (modulo formatting) to what
/// the previous inline match produced, so users see no behaviour
/// change when an unknown `fs_type` is configured.
pub fn select_formatter(
    fs_type: &str,
    image_name: &str,
    partition_name: &str,
) -> Result<&'static dyn PartitionFormatter> {
    match fs_type {
        "fat32" => Ok(&crate::image::fat::Fat32Formatter),
        #[cfg(test)]
        "stub" => Ok(&tests::StubFormatter),
        other => Err(Error::Compile(format!(
            "image('{image_name}') partition('{partition_name}'): \
             fs_type '{other}' is not implemented. \
             Supported in Phase 3: \"fat32\"."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only formatter that records its arguments instead of
    /// writing anything. Validates the trait shape: confirms that
    /// a third-party impl can be slotted in without touching the
    /// dispatcher's existing arms.
    pub(super) struct StubFormatter;

    impl PartitionFormatter for StubFormatter {
        fn format_in_place(
            &self,
            _image_path: &Path,
            _start_byte: u64,
            _len_bytes: u64,
            scoped_name: &str,
            entries: &[ResolvedEntry],
            stdout: &mut Vec<u8>,
        ) -> Result<()> {
            use std::io::Write;
            let _ = writeln!(
                stdout,
                "    [stub] {scoped_name}: {} entries",
                entries.len()
            );
            Ok(())
        }
    }

    #[test]
    fn select_formatter_returns_fat32_for_fat32() {
        let f = select_formatter("fat32", "img", "p1").unwrap();
        // The trait object's address should match Fat32Formatter's
        // static. We can't compare trait-objects directly, so
        // round-trip via a no-op call into the stub-friendly path.
        let mut stdout = Vec::new();
        let result = f.format_in_place(
            std::path::Path::new("/nonexistent/path/that/cannot/exist"),
            0,
            512,
            "img::partition(p1)",
            &[],
            &mut stdout,
        );
        // FAT32 will error trying to open the nonexistent path —
        // that confirms we got the FAT32 impl, not the stub.
        assert!(result.is_err(), "expected FAT32 to fail on nonexistent path");
    }

    #[test]
    fn select_formatter_returns_stub_in_test_cfg() {
        let f = select_formatter("stub", "img", "p1").unwrap();
        let mut stdout = Vec::new();
        f.format_in_place(
            std::path::Path::new("/dev/null"),
            0,
            0,
            "img::partition(p1)",
            &[],
            &mut stdout,
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&stdout).contains("[stub] img::partition(p1)"),
            "stub formatter must record the scoped name in stdout"
        );
    }

    #[test]
    fn select_formatter_unknown_fs_type_errors_with_partition_name() {
        let err = match select_formatter("xfs", "myimg", "data") {
            Err(e) => e,
            Ok(_) => panic!("expected an error for unknown fs_type"),
        };
        let msg = err.to_string();
        assert!(msg.contains("myimg"), "error must name the image");
        assert!(msg.contains("data"), "error must name the partition");
        assert!(msg.contains("xfs"), "error must name the bad fs_type");
        assert!(msg.contains("fat32"), "error must list supported types");
    }
}
