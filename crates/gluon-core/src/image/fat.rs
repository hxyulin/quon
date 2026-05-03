//! FAT32 image writer (hadris-fat wrapper).
//!
//! Phase 2 scope: take a list of `(dest_path, source_path)` entries
//! and produce a single FAT32-formatted image at `image_path` whose
//! contents match. Dest paths use forward slashes (FAT semantics);
//! intermediate directories are created on demand.
//!
//! Phase 3 will reuse this module from inside a partition write —
//! the partition builder will hand us a byte-range writer rather
//! than a filesystem path. Today we work directly on a `File`.

use crate::error::{Error, Result};
use hadris_fat::format::{FatVolumeFormatter, FormatOptions};
use hadris_fat::sync::FatFs;
use hadris_fat::sync::write::FatFsWriteExt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::ResolvedEntry;
use super::source::StampSource;

/// Format `image_path` as a flat FAT32 image (no partition table).
///
/// Preconditions:
/// - `size_bytes` must be at least the FAT32 minimum (~33 MiB).
///   `compute_size_bytes` in `mod.rs` floors at 64 MiB to avoid this.
/// - Every `entry.source_path` must exist.
///
/// Side effect: truncates and rewrites `image_path`.
pub(super) fn format_fat32(
    image_path: &Path,
    size_bytes: u64,
    image_name: &str,
    entries: &[ResolvedEntry],
    stdout: &mut Vec<u8>,
) -> Result<()> {
    // Open or create + truncate the image file. We always write from
    // scratch: an incremental rewrite with hadris would require us to
    // honour pre-existing FS state, and given the cache already
    // short-circuits unchanged builds, the simpler "blow it away and
    // rebuild" path is correct.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(image_path)
        .map_err(|e| Error::Io {
            path: image_path.to_path_buf(),
            source: e,
        })?;

    file.set_len(size_bytes).map_err(|e| Error::Io {
        path: image_path.to_path_buf(),
        source: e,
    })?;

    format_fat32_into(file, size_bytes, image_name, entries, stdout)
}

/// Format an arbitrary `Read + Write + Seek` (a whole-file handle, or
/// a [`crate::image::partition::PartitionWindow`] view) as FAT32 with
/// the given byte size, then write every entry into it.
///
/// The partitioned-disk path uses this directly: each FAT32 partition
/// is handed a fresh `PartitionWindow` over its byte range, and the
/// formatter operates as if the partition were a standalone disk.
pub(super) fn format_fat32_into<DATA>(
    data: DATA,
    size_bytes: u64,
    image_name: &str,
    entries: &[ResolvedEntry],
    stdout: &mut Vec<u8>,
) -> Result<()>
where
    DATA: hadris_fat::sync::Read + hadris_fat::sync::Write + hadris_fat::sync::Seek,
{
    // Volume label: FAT32 caps at 11 ASCII chars, uppercased. We
    // intentionally do NOT use the image name here. Hadris (like most
    // FAT formatters) writes the volume label as a special directory
    // entry in the root directory with the volume_id attribute set.
    // Hadris's `find()` walks all root entries — including the volume
    // label entry — which would produce a "non-directory of that name
    // already exists" error if the user's image happened to be named
    // the same as a top-level directory in their dest_paths (e.g.
    // `image("boot")` packing files under `/boot/...`).
    //
    // A fixed label sidesteps this entirely. Volume labels aren't
    // load-bearing for boot — UEFI looks at the FS type byte, not the
    // label. Users who want a custom label can override via a future
    // `image().volume_label("...")` builder once the collision-free
    // listing API exists.
    let options = FormatOptions::new(size_bytes).with_label("GLUON");

    let fs = FatVolumeFormatter::format(data, options).map_err(|e| {
        Error::Compile(format!(
            "image('{}'): FAT32 format failed (size={} bytes): {e}",
            image_name, size_bytes
        ))
    })?;

    // Write every entry. Order matters for determinism: entries are in
    // the order the user wrote them in gluon.rs (preserved by
    // ImageDef::entries: Vec<ImageEntry>), and FAT directory ordering
    // follows file-creation order, so the byte layout is reproducible
    // across runs assuming inputs match.
    for entry in entries {
        write_one_entry(&fs, entry, image_name)?;
        let _ = writeln!(
            stdout,
            "    Packed {} -> image/{}/{}",
            describe_source(&entry.source_kind),
            image_name,
            entry.dest_path
        );
    }

    Ok(())
}

fn write_one_entry<DATA>(fs: &FatFs<DATA>, entry: &ResolvedEntry, image_name: &str) -> Result<()>
where
    DATA: hadris_fat::sync::Read + hadris_fat::sync::Write + hadris_fat::sync::Seek,
{
    match &entry.source_kind {
        StampSource::EspDir { .. } => {
            // ESP source: walk the directory and recreate the layout
            // under `entry.dest_path`. The dest_path is the prefix
            // *inside* the FAT image. An ESP source typically has
            // dest_path = "/" (mount the ESP at the root) or
            // dest_path = "/EFI" (graft it in).
            let prefix = normalise_dest(&entry.dest_path);
            walk_and_write(fs, &entry.source_path, &prefix, image_name)?;
        }
        _ => {
            // Single file. Read into memory (boot artifacts and configs
            // are small — we don't optimise for multi-GB streams).
            let bytes = std::fs::read(&entry.source_path).map_err(|e| Error::Io {
                path: entry.source_path.clone(),
                source: e,
            })?;
            write_file_at_path(fs, &entry.dest_path, &bytes, image_name)?;
        }
    }
    Ok(())
}

fn walk_and_write<DATA>(
    fs: &FatFs<DATA>,
    src_root: &Path,
    dest_prefix: &str,
    image_name: &str,
) -> Result<()>
where
    DATA: hadris_fat::sync::Read + hadris_fat::sync::Write + hadris_fat::sync::Seek,
{
    for entry in walkdir::WalkDir::new(src_root).sort_by_file_name() {
        let entry = entry.map_err(|e| Error::Compile(format!("walkdir: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        // Build the dest path inside the FAT image.
        let rel = entry
            .path()
            .strip_prefix(src_root)
            .map_err(|e| Error::Compile(format!("strip_prefix: {e}")))?;
        let rel_str = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let dest = if dest_prefix.is_empty() || dest_prefix == "/" {
            format!("/{rel_str}")
        } else {
            format!("{}/{}", dest_prefix.trim_end_matches('/'), rel_str)
        };
        let bytes = std::fs::read(entry.path()).map_err(|e| Error::Io {
            path: entry.path().to_path_buf(),
            source: e,
        })?;
        write_file_at_path(fs, &dest, &bytes, image_name)?;
    }
    Ok(())
}

/// Create any missing intermediate directories in `dest_path`, then
/// create the final file and write `bytes`.
fn write_file_at_path<DATA>(
    fs: &FatFs<DATA>,
    dest_path: &str,
    bytes: &[u8],
    image_name: &str,
) -> Result<()>
where
    DATA: hadris_fat::sync::Read + hadris_fat::sync::Write + hadris_fat::sync::Seek,
{
    let normalised = normalise_dest(dest_path);
    let components: Vec<&str> = normalised.split('/').filter(|s| !s.is_empty()).collect();
    if components.is_empty() {
        return Err(Error::Compile(format!(
            "image('{}'): entry dest_path '{}' has no filename component",
            image_name, dest_path
        )));
    }
    let (file_name, dir_components) = components.split_last().expect("non-empty");

    // Walk down, creating dirs as needed. `fs.root_dir()` is cheap; we
    // re-fetch it each time rather than threading the FatDir through
    // recursion (its lifetime is tied to `fs`, awkward to pass).
    let mut cur = fs.root_dir();
    for &comp in dir_components {
        // `find` reports whether the entry exists already; if it does
        // and is a directory, descend into it. Otherwise create it.
        match cur
            .find(comp)
            .map_err(|e| fat_to_compile(image_name, dest_path, e))?
        {
            Some(existing) if existing.is_directory() => {
                cur = cur
                    .open_dir(comp)
                    .map_err(|e| fat_to_compile(image_name, dest_path, e))?;
            }
            Some(_) => {
                return Err(Error::Compile(format!(
                    "image('{}'): dest_path '{}' tries to descend into '{}' but a non-directory entry by that name already exists",
                    image_name, dest_path, comp
                )));
            }
            None => {
                cur = fs
                    .create_dir(&cur, comp)
                    .map_err(|e| fat_to_compile(image_name, dest_path, e))?;
            }
        }
    }

    // Final file: error out if it already exists; we treat duplicate
    // dest_paths as a config error rather than silently overwriting.
    if cur
        .find(file_name)
        .map_err(|e| fat_to_compile(image_name, dest_path, e))?
        .is_some()
    {
        return Err(Error::Compile(format!(
            "image('{}'): duplicate dest_path '{}' (a file or directory by that name was already added)",
            image_name, dest_path
        )));
    }

    let entry = fs
        .create_file(&cur, file_name)
        .map_err(|e| fat_to_compile(image_name, dest_path, e))?;
    let mut writer = fs
        .write_file(&entry)
        .map_err(|e| fat_to_compile(image_name, dest_path, e))?;
    let mut written = 0usize;
    while written < bytes.len() {
        let n = writer
            .write(&bytes[written..])
            .map_err(|e| fat_to_compile(image_name, dest_path, e))?;
        if n == 0 {
            return Err(Error::Compile(format!(
                "image('{}'): FAT writer accepted 0 bytes for dest_path '{}' \
                 — out of space? (size_bytes too small relative to entry payload)",
                image_name, dest_path
            )));
        }
        written += n;
    }
    writer
        .finish()
        .map_err(|e| fat_to_compile(image_name, dest_path, e))?;
    Ok(())
}

fn normalise_dest(dest: &str) -> String {
    // Always work with forward slashes. Strip a leading `/` so we can
    // split on `/` cleanly.
    let s = dest.replace('\\', "/");
    s.trim_start_matches('/').to_string()
}

fn describe_source(s: &StampSource) -> String {
    match s {
        StampSource::Crate { name } => format!("crate({name})"),
        StampSource::EspDir { name } => format!("esp({name})"),
        StampSource::File { path } => format!("file({path})"),
    }
}

fn fat_to_compile(image_name: &str, dest_path: &str, err: hadris_fat::FatError) -> Error {
    Error::Compile(format!(
        "image('{}') FAT write to '{}': {err}",
        image_name, dest_path
    ))
}

// Suppress unused-import warning if hadris-fat ever changes its
// re-export surface and one of these imports stops being needed.
#[allow(dead_code)]
fn _hold_imports() -> (PathBuf,) {
    (PathBuf::new(),)
}
