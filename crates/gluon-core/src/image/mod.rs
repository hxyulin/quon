//! Disk image assembly.
//!
//! [`build_image`] produces a single `<build>/images/<name>.img` file
//! from an [`ImageDef`]. Phase 2 supports two formats:
//!
//! - **fat32**: a single FAT32 filesystem image, with each entry's
//!   destination path created inside the FS. Backed by `hadris-fat`.
//! - **raw**: empty. Phase 2 deliberately rejects raw, because it
//!   has no sensible interpretation when the image carries multiple
//!   entries with `dest_path`s. Raw byte placement at LBA offsets
//!   lands in Phase 3 via `ImageDef::raw_writes`.
//!
//! Future phases will add: MBR/GPT partition tables ([`ImageDef::partition_table`]),
//! sub-partition builders ([`ImageDef::partitions`]), raw LBA writes
//! ([`ImageDef::raw_writes`]), and the Stage1Meta blob.
//!
//! Image builds have one fingerprint record, keyed `image:<name>`. Its
//! command hashes the serialised `ImageDef`, the `hadris-fat` version
//! and the per-entry source kinds; its declared inputs are every file
//! that goes into the image. See `docs/src/architecture/cache.md`.

mod fat;
mod formatter;
mod mbr;
mod partition;
mod source;

use crate::cache::{Freshness, FreshnessQuery, RecordInput, hash_parts};
use crate::compile::CompileCtx;
use crate::compile::extern_map::ArtifactLookup;
use crate::compile::extern_map::resolve_artifact_by_name;
use crate::error::{Error, Result};
use gluon_model::{
    BuildModel, Handle, ImageDef, ImageFormat, ImageSource, PartitionDef, PartitionTable,
    RawSource, RawWrite, ResolvedConfig,
};
use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use mbr::{ResolvedPartition, SECTOR_SIZE};
use source::StampSource;

/// Inputs the image dispatcher needs from the scheduler.
///
/// A fresh struct rather than positional args because the contents will
/// grow in later phases (raw-write source resolution, ESP path lookup,
/// nested-partition layout) and renaming positional parameters churns
/// the call sites.
pub struct ImageCtx<'a> {
    /// Project layout (used for `image_path` and `project_root`).
    pub ctx: &'a CompileCtx,
    /// Build model — needed to resolve `ImageSource::Crate(name)` /
    /// `ImageSource::Esp(name)` to handles.
    pub model: &'a BuildModel,
    /// Resolved config — gives us `project_root` for `ImageSource::File`
    /// resolution.
    pub resolved: &'a ResolvedConfig,
    /// Outputs of finished nodes: crate artifacts, build-step outputs
    /// and assembled ESP directories.
    pub artifacts: &'a dyn ArtifactLookup,
}

/// Assemble the disk image for one [`ImageDef`].
///
/// Returns the output path and a `was_cached` flag. The path is the
/// canonical `<build>/images/<name>.img` location regardless of cache
/// status; the flag controls how the build summary counters are
/// updated.
///
/// Caller invariants (the DAG enforces them):
/// - Every `ImageSource::Crate` referenced by a resolvable entry has
///   already been compiled — its handle is present in `artifacts`.
/// - Every `ImageSource::Esp` referenced has been assembled — its
///   handle is present in `esp_dirs`.
/// - `ImageSource::File` sources exist on the host filesystem (we
///   surface a clear `Error::Io` if they don't).
pub fn build_image(
    image_handle: Handle<ImageDef>,
    ictx: &ImageCtx<'_>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let image = ictx.model.images.get(image_handle).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: image handle {:?} not found in model.images",
            image_handle
        ))
    })?;

    let image_path = ictx.ctx.layout.image_path(&image.name);

    // Resolve every input. The two image shapes (flat-FS vs
    // partitioned disk) have different resolution surfaces, so each
    // gets its own resolved-entries vec — but both feed the same
    // record's declared inputs so invalidation is uniform.
    let flat_entries = resolve_entries(image, ictx)?;
    let partition_resolutions = resolve_partition_entries(image, ictx)?;
    let raw_resolutions = resolve_raw_writes(image, ictx)?;

    // Declared inputs include flat entries, partition entries, and
    // raw writes — every byte that contributes to the final image
    // needs a fingerprint or a `touch` on the relevant source won't
    // invalidate. We materialise into one `Vec<ResolvedEntry>` so the
    // per-entry source-kind tags (used for the command hash) and the
    // source paths (used for the cache's input set) come from a
    // single iteration. Raw writes get a synthetic dest_path keyed on
    // their LBA so they show up distinctly.
    let mut stamp_inputs: Vec<ResolvedEntry> = flat_entries.clone();
    for partition in &partition_resolutions {
        stamp_inputs.extend(partition.entries.iter().cloned());
    }
    stamp_inputs.extend(raw_resolutions.iter().filter_map(|r| r.as_resolved_entry()));

    // Compute the command hash from (serialised ImageDef without
    // span) + hadris-fat version + per-entry source kinds. Source kinds
    // are folded in so a crate→file rename in `gluon.rs` invalidates
    // even when the resolved on-disk path happens to match.
    let mut def_for_hash = image.clone();
    def_for_hash.span = None;
    let def_bytes = serde_json::to_vec(&def_for_hash).map_err(|e| {
        Error::Compile(format!(
            "image('{}'): failed to serialise ImageDef for cache key: {e}",
            image.name
        ))
    })?;
    let mut source_kind_bytes = Vec::new();
    for e in &stamp_inputs {
        source_kind_bytes.extend_from_slice(&e.source_kind.aux_bytes());
        source_kind_bytes.push(0);
    }
    let command = hash_parts(
        "gluon.image.v1",
        &[
            &def_bytes,
            hadris_fat_version().as_bytes(),
            &source_kind_bytes,
        ],
    );

    // Declared inputs: every source path that contributes to the
    // final image. Sort + dedup for deterministic query ordering and
    // to defeat duplicate-entry input lists.
    let mut input_paths: Vec<PathBuf> =
        stamp_inputs.iter().map(|e| e.source_path.clone()).collect();
    input_paths.sort();
    input_paths.dedup();

    let key = format!("image:{}", image.name);
    let toolchain = ictx.ctx.toolchain();
    let outputs = [image_path.clone()];
    let no_env = BTreeMap::new();
    let query = FreshnessQuery {
        key: &key,
        command,
        toolchain,
        declared: &input_paths,
        outputs: &outputs,
        command_env: &no_env,
    };
    match ictx.ctx.store.check(&query) {
        Freshness::Fresh(_) => return Ok((image_path, true)),
        Freshness::Stale(reason) => {
            ictx.ctx
                .report_stale(stderr, &format!("image {}", image.name), &reason)
        }
    }
    let started_at = std::time::SystemTime::now();

    // Slow path. Make sure the parent dir exists.
    if let Some(parent) = image_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    if let Some(table) = image.partition_table {
        // Partitioned-disk path. `entries` belongs in `partitions`
        // here — a shape error worth catching loudly.
        if !image.entries.is_empty() {
            return Err(Error::Compile(format!(
                "image('{}') has partition_table set but also has top-level entries. \
                 Top-level entries are for flat-FS images; move them inside a \
                 partition(...) block.",
                image.name
            )));
        }
        if image.partitions.is_empty() && image.raw_writes.is_empty() {
            return Err(Error::Compile(format!(
                "image('{}') has partition_table set but neither partitions nor raw_writes. \
                 An empty disk image isn't useful — declare at least one partition or \
                 write_*_at(...) before building.",
                image.name
            )));
        }
        build_partitioned_disk(
            image,
            &image_path,
            table,
            &partition_resolutions,
            &raw_resolutions,
            stdout,
        )?;
    } else {
        // Flat-FS path. Format dispatch defaults to FAT32 — that's the
        // realistic case (every UEFI ESP). `raw` without a partition
        // table makes no sense (no FS, no LBA layout — what would it
        // mean?), so we still reject it here. The other variants
        // (FAT12/FAT16/ISO9660) aren't implemented yet; they round-trip
        // through the model but error out at build time so the user
        // gets a clear "supported flat-FS: fat32" message.
        let format = image.format.unwrap_or(ImageFormat::Fat32);
        match format {
            ImageFormat::Fat32 => {
                let size_bytes = compute_size_bytes(image, &flat_entries)?;
                fat::format_fat32(&image_path, size_bytes, &image.name, &flat_entries, stdout)?;
            }
            ImageFormat::Raw => {
                return Err(Error::Config(format!(
                    "image(\"{}\").format(\"raw\") without a partition table has no \
                     well-defined layout. To produce a partitioned raw disk, call \
                     .partition_table(\"mbr\") and place data via .write_*_at(...) / \
                     .partition(...).",
                    image.name
                )));
            }
            other => {
                return Err(Error::Config(format!(
                    "image(\"{}\").format(\"{}\"): unsupported format. \
                     Supported flat-FS: \"fat32\". For partitioned disks, set \
                     .partition_table(\"mbr\") instead of .format.",
                    image.name,
                    other.as_str(),
                )));
            }
        }
    }

    // Recorded only after the write succeeds: an interrupted run that
    // left a half-written image rebuilds next time.
    ictx.ctx.store.record(RecordInput {
        key: &key,
        command,
        toolchain,
        declared: &input_paths,
        discovered: &[],
        env: BTreeMap::new(),
        outputs: &outputs,
        started_at,
        stderr: &[],
    })?;

    Ok((image_path, false))
}

/// Per-partition resolved view: the partition def (with end_lba
/// finalised), plus the resolved entries to write inside it.
struct ResolvedPartitionEntries<'a> {
    def: &'a PartitionDef,
    entries: Vec<ResolvedEntry>,
}

/// Resolved raw write: byte source plus the LBA the user pinned it to.
struct ResolvedRawWrite<'a> {
    write: &'a RawWrite,
    /// In-memory bytes for the write. We materialise eagerly because
    /// (a) raw_writes are small (boot sector ≤ 446B, stage 1.x ≤ 32 KiB),
    /// and (b) the same bytes feed both the stamp fingerprint and the
    /// final write — reading twice would double the I/O for no benefit.
    bytes: Vec<u8>,
    /// `(source_path, source_kind)` — used by the stamp. None for inline
    /// `RawSource::Bytes`, because the bytes themselves are part of the
    /// serialised `ImageDef` and are already covered by `def_hash`.
    fingerprint: Option<(PathBuf, StampSource)>,
}

impl<'a> ResolvedRawWrite<'a> {
    fn as_resolved_entry(&self) -> Option<ResolvedEntry> {
        self.fingerprint.as_ref().map(|(path, kind)| ResolvedEntry {
            // Sentinel dest_path: raw writes don't live in any FS, but
            // the stamp keys by dest_path so we synthesise one. The
            // LBA is unique per raw_write, so this also suffices for
            // the BTreeMap key in stamp.rs.
            dest_path: format!("__raw__@lba={}", self.write.lba),
            source_path: path.clone(),
            source_kind: kind.clone(),
        })
    }
}

/// Per-entry resolved view used by the format implementations.
#[derive(Clone)]
pub(crate) struct ResolvedEntry {
    pub dest_path: String,
    pub source_path: PathBuf,
    /// Tag for the stamp/diagnostic, not the FS write itself.
    pub source_kind: StampSource,
}

fn resolve_entries(image: &ImageDef, ictx: &ImageCtx<'_>) -> Result<Vec<ResolvedEntry>> {
    let mut out = Vec::with_capacity(image.entries.len());
    for entry in &image.entries {
        match &entry.source {
            ImageSource::Crate(name) => {
                // Unified artifact namespace: a name may resolve to a
                // crate or to a build step. Validate has already
                // rejected collisions, so at most one resolves.
                // Unresolved names are silently skipped — the validate
                // pass should already have diagnosed them; if not, the
                // image will be built without that source and the user
                // will see the missing data downstream.
                let path = match resolve_artifact_by_name(ictx.model, ictx.artifacts, name) {
                    Some(p) => p,
                    None => {
                        // Distinguish "name unknown to model" (silently
                        // skipped) from "name known but artifact not
                        // produced yet" (scheduler ordering bug). The
                        // distinction matters because the latter is a
                        // bug we want to surface loudly.
                        let known_in_model = ictx.model.crates.lookup(name).is_some()
                            || ictx.model.build_steps.lookup(name).is_some();
                        if known_in_model {
                            return Err(Error::Compile(format!(
                                "scheduler: image('{}') entry source '{}' has no entry in \
                                 ArtifactMap (DAG edge missing or ran out of order?)",
                                image.name, name
                            )));
                        }
                        continue;
                    }
                };
                out.push(ResolvedEntry {
                    dest_path: entry.dest_path.clone(),
                    source_path: path.to_path_buf(),
                    source_kind: StampSource::Crate { name: name.clone() },
                });
            }
            ImageSource::Esp(name) => {
                let handle = match ictx.model.esps.lookup(name) {
                    Some(h) => h,
                    None => continue,
                };
                let dir = ictx.artifacts.esp(handle).ok_or_else(|| {
                    Error::Compile(format!(
                        "scheduler: image('{}') entry source esp '{}' has no \
                         assembled directory (DAG edge missing or ran out of order?)",
                        image.name, name
                    ))
                })?;
                // For an ESP source, the "source path" is the directory
                // tree. We mark it specially so the FAT writer can
                // walk-and-copy instead of treating it as a single file.
                out.push(ResolvedEntry {
                    dest_path: entry.dest_path.clone(),
                    source_path: dir.to_path_buf(),
                    source_kind: StampSource::EspDir { name: name.clone() },
                });
            }
            ImageSource::File(path) => {
                // Host file: relative to project root, like other
                // file-path fields in the model.
                let abs = ictx.resolved.project_root.join(path);
                if !abs.exists() {
                    return Err(Error::Compile(format!(
                        "image('{}') add_file('{}', '{}'): host file does not exist \
                         at {} (paths are relative to the project root)",
                        image.name,
                        path,
                        entry.dest_path,
                        abs.display()
                    )));
                }
                out.push(ResolvedEntry {
                    dest_path: entry.dest_path.clone(),
                    source_path: abs,
                    source_kind: StampSource::File { path: path.clone() },
                });
            }
        }
    }
    Ok(out)
}

/// Resolve every partition's `entries` to ([`ResolvedEntry`]) form,
/// preserving the partition definition for the dispatcher. ESP / crate
/// / file resolution mirrors [`resolve_entries`].
fn resolve_partition_entries<'a>(
    image: &'a ImageDef,
    ictx: &ImageCtx<'_>,
) -> Result<Vec<ResolvedPartitionEntries<'a>>> {
    let mut out = Vec::with_capacity(image.partitions.len());
    for part in &image.partitions {
        // We re-use `resolve_entries`'s logic by constructing a
        // throwaway ImageDef with just the partition's entries. This
        // keeps the resolution logic in one place; the per-partition
        // diagnostic context (image name + partition name) is added
        // here via the error wrapping below.
        let scratch = ImageDef {
            name: format!("{}::partition({})", image.name, part.name),
            entries: part.entries.clone(),
            ..Default::default()
        };
        let entries = resolve_entries(&scratch, ictx)?;
        out.push(ResolvedPartitionEntries { def: part, entries });
    }
    Ok(out)
}

/// Materialise every `RawWrite` source into bytes plus a fingerprint.
fn resolve_raw_writes<'a>(
    image: &'a ImageDef,
    ictx: &ImageCtx<'_>,
) -> Result<Vec<ResolvedRawWrite<'a>>> {
    let mut out = Vec::with_capacity(image.raw_writes.len());
    for rw in &image.raw_writes {
        match &rw.source {
            RawSource::Crate { name } => {
                // Unified namespace: try crate then build_step.
                let path = match resolve_artifact_by_name(ictx.model, ictx.artifacts, name) {
                    Some(p) => p,
                    None => {
                        let known_in_model = ictx.model.crates.lookup(name).is_some()
                            || ictx.model.build_steps.lookup(name).is_some();
                        if known_in_model {
                            return Err(Error::Compile(format!(
                                "scheduler: image('{}') write_crate_at({}, '{}'): no entry \
                                 in ArtifactMap (DAG edge missing or ran out of order?)",
                                image.name, rw.lba, name
                            )));
                        }
                        continue;
                    }
                };
                let bytes = std::fs::read(path).map_err(|e| Error::Io {
                    path: path.to_path_buf(),
                    source: e,
                })?;
                out.push(ResolvedRawWrite {
                    write: rw,
                    bytes,
                    fingerprint: Some((
                        path.to_path_buf(),
                        StampSource::Crate { name: name.clone() },
                    )),
                });
            }
            RawSource::File { path } => {
                let abs = ictx.resolved.project_root.join(path);
                if !abs.exists() {
                    return Err(Error::Compile(format!(
                        "image('{}') write_file_at({}, '{}'): host file does not exist at {}",
                        image.name,
                        rw.lba,
                        path,
                        abs.display()
                    )));
                }
                let bytes = std::fs::read(&abs).map_err(|e| Error::Io {
                    path: abs.clone(),
                    source: e,
                })?;
                out.push(ResolvedRawWrite {
                    write: rw,
                    bytes,
                    fingerprint: Some((abs, StampSource::File { path: path.clone() })),
                });
            }
            RawSource::Bytes { bytes } => {
                out.push(ResolvedRawWrite {
                    write: rw,
                    bytes: bytes.clone(),
                    // Inline bytes are already part of the serialised
                    // ImageDef, so `def_hash` covers their content. No
                    // separate fingerprint needed.
                    fingerprint: None,
                });
            }
        }
    }
    Ok(out)
}

/// Build a partitioned hybrid disk: zero-fill the file at the chosen
/// size, write each `RawWrite` at its LBA, format every FAT32 partition
/// in place via a [`PartitionWindow`], and finally stamp the partition
/// table into sector 0 (last so it sits on top of any boot-sector code
/// supplied via a `RawWrite` at LBA 0).
fn build_partitioned_disk(
    image: &ImageDef,
    image_path: &Path,
    table: PartitionTable,
    partitions: &[ResolvedPartitionEntries<'_>],
    raw_writes: &[ResolvedRawWrite<'_>],
    stdout: &mut Vec<u8>,
) -> Result<()> {
    // Phase 3 implements MBR; GPT is reserved.
    if !matches!(table, PartitionTable::Mbr) {
        return Err(Error::Compile(format!(
            "image('{}'): partition_table value not implemented (Phase 3 supports MBR only)",
            image.name
        )));
    }

    // Pick the image size. If the user pinned `size_mb`, honour it;
    // otherwise size to fit the largest LBA written.
    let size_bytes = compute_partitioned_size_bytes(image, partitions, raw_writes)?;
    let total_sectors = size_bytes / SECTOR_SIZE;

    // Resolve any open-ended `end_lba: None` partitions to the actual
    // last sector of the image, and validate the layout. This is the
    // hard checkpoint between the "user-described" and "byte-addressed"
    // worlds — once we get past it, every following write is at a
    // concrete byte offset.
    let resolved_parts = resolve_partition_lbas(image, partitions, total_sectors)?;
    validate_layout(image, &resolved_parts, raw_writes, total_sectors)?;
    if resolved_parts.len() > 4 {
        return Err(Error::Compile(format!(
            "image('{}'): MBR allows at most 4 primary partitions, got {}",
            image.name,
            resolved_parts.len()
        )));
    }

    // Step 1: create / truncate the image at full size, zero-filled.
    {
        let file = std::fs::OpenOptions::new()
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
    }

    // Step 2: raw_writes. We do these BEFORE the MBR is stamped so
    // that a raw_write at LBA 0 supplying the BIOS boot sector code
    // (bytes 0..446) doesn't get clobbered.
    for r in raw_writes {
        write_raw(image_path, image, r)?;
        let _ = writeln!(
            stdout,
            "    Wrote {} bytes at LBA {} ({})",
            r.bytes.len(),
            r.write.lba,
            describe_raw_source(&r.write.source),
        );
    }

    // Step 3: format each partition's filesystem in place via a
    // window over the disk.
    for rp in &resolved_parts {
        format_partition_in_place(image_path, image, rp, partitions, stdout)?;
    }

    // Step 4: stamp the MBR partition table + signature at sector 0.
    // This MUST come last so it sits on top of any boot-sector code
    // a `RawWrite` at LBA 0 supplied — the table lives at bytes
    // 446..510 + 0x55AA at 510..512, leaving bytes 0..446 to the boot
    // code.
    let mbr_partitions: Vec<ResolvedPartition<'_>> = resolved_parts
        .iter()
        .map(|rp| ResolvedPartition {
            def: rp.def,
            end_lba: rp.end_lba,
        })
        .collect();
    mbr::write_mbr(image_path, &image.name, &mbr_partitions)?;
    let _ = writeln!(
        stdout,
        "    Stamped MBR partition table ({} entries)",
        mbr_partitions.len()
    );

    Ok(())
}

/// Resolution of `end_lba: None` to a concrete sector index. We don't
/// rewrite the model — the resolved value lives only in this temporary
/// view that the layout pass and the writer share.
struct ResolvedPart<'a> {
    def: &'a PartitionDef,
    end_lba: u64,
    /// Index back into the caller's `partitions` slice so we can reach
    /// the resolved entries when formatting.
    src_index: usize,
}

fn resolve_partition_lbas<'a>(
    image: &ImageDef,
    partitions: &'a [ResolvedPartitionEntries<'a>],
    total_sectors: u64,
) -> Result<Vec<ResolvedPart<'a>>> {
    let mut out = Vec::with_capacity(partitions.len());
    for (i, rp) in partitions.iter().enumerate() {
        let end_lba = match rp.def.end_lba {
            Some(e) => e,
            None => {
                if total_sectors == 0 {
                    return Err(Error::Compile(format!(
                        "image('{}') partition('{}'): end_lba is open-ended but image \
                         size resolves to zero sectors",
                        image.name, rp.def.name
                    )));
                }
                total_sectors - 1
            }
        };
        if end_lba < rp.def.start_lba {
            return Err(Error::Compile(format!(
                "image('{}') partition('{}'): end_lba {} is before start_lba {}",
                image.name, rp.def.name, end_lba, rp.def.start_lba
            )));
        }
        out.push(ResolvedPart {
            def: rp.def,
            end_lba,
            src_index: i,
        });
    }
    Ok(out)
}

/// Validate that no two writes (raw_writes / partitions / MBR sector)
/// overlap, and every byte written fits within the image. Diagnostics
/// quote the offending span.
fn validate_layout(
    image: &ImageDef,
    partitions: &[ResolvedPart<'_>],
    raw_writes: &[ResolvedRawWrite<'_>],
    total_sectors: u64,
) -> Result<()> {
    // Half-open sector range for each write.
    struct SectorRange {
        start: u64,
        end: u64, // exclusive
        what: String,
    }
    let mut ranges: Vec<SectorRange> = Vec::new();

    for r in raw_writes {
        let bytes = r.bytes.len() as u64;
        let sectors = bytes.div_ceil(SECTOR_SIZE);
        if let Some(cap) = r.write.max_sectors {
            if sectors > cap {
                return Err(Error::Compile(format!(
                    "image('{}') write_*_at(lba={}): payload occupies {} sectors but \
                     max_sectors={} was set ({} bytes / {} sector)",
                    image.name, r.write.lba, sectors, cap, bytes, SECTOR_SIZE
                )));
            }
        }
        let end = r.write.lba + sectors;
        if end > total_sectors {
            return Err(Error::Compile(format!(
                "image('{}') write_*_at(lba={}): payload extends past image \
                 (would need {} sectors total, image has {})",
                image.name, r.write.lba, end, total_sectors
            )));
        }
        ranges.push(SectorRange {
            start: r.write.lba,
            end,
            what: format!("write_*_at(lba={})", r.write.lba),
        });
    }

    for p in partitions {
        if p.end_lba >= total_sectors {
            return Err(Error::Compile(format!(
                "image('{}') partition('{}'): end_lba={} exceeds last sector ({})",
                image.name,
                p.def.name,
                p.end_lba,
                total_sectors.saturating_sub(1)
            )));
        }
        ranges.push(SectorRange {
            start: p.def.start_lba,
            end: p.end_lba + 1,
            what: format!("partition('{}')", p.def.name),
        });
    }

    // Pairwise overlap check. O(n²) is fine — we cap MBR at 4
    // partitions and raw_writes are typically a handful.
    for i in 0..ranges.len() {
        for j in (i + 1)..ranges.len() {
            let a = &ranges[i];
            let b = &ranges[j];
            if a.start < b.end && b.start < a.end {
                return Err(Error::Compile(format!(
                    "image('{}'): {} overlaps {} (sectors [{}, {}) vs [{}, {}))",
                    image.name, a.what, b.what, a.start, a.end, b.start, b.end
                )));
            }
        }
    }

    Ok(())
}

fn compute_partitioned_size_bytes(
    image: &ImageDef,
    partitions: &[ResolvedPartitionEntries<'_>],
    raw_writes: &[ResolvedRawWrite<'_>],
) -> Result<u64> {
    if let Some(mb) = image.size_mb {
        return Ok((mb as u64) * 1024 * 1024);
    }
    // Auto-size: extend to cover the largest LBA + sector count among
    // writes. This is intentionally conservative (the ESP is usually
    // open-ended via `end_lba: None` so the user can sidestep this
    // path entirely by setting `size_mb`), but avoids producing a
    // hilariously small disk by accident.
    let mut max_sector: u64 = 1; // MBR itself
    for r in raw_writes {
        let n = (r.bytes.len() as u64).div_ceil(SECTOR_SIZE);
        max_sector = max_sector.max(r.write.lba + n);
    }
    for p in partitions {
        if let Some(end) = p.def.end_lba {
            max_sector = max_sector.max(end + 1);
        } else {
            // Open-ended partition forces *some* concrete cap. With no
            // user-supplied size_mb, we floor at 64 MiB as in the
            // flat-FS path — gives an open-ended ESP comfortable room.
            max_sector = max_sector.max((64 * 1024 * 1024) / SECTOR_SIZE);
        }
    }
    let bytes = max_sector * SECTOR_SIZE;
    // Round up to a multiple of 1 MiB so e.g. `qemu`'s drive geometry
    // detection doesn't spit warnings.
    const MIB: u64 = 1024 * 1024;
    Ok(bytes.div_ceil(MIB) * MIB)
}

fn write_raw(image_path: &Path, _image: &ImageDef, r: &ResolvedRawWrite<'_>) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image_path)
        .map_err(|e| Error::Io {
            path: image_path.to_path_buf(),
            source: e,
        })?;
    let byte_off = r.write.lba * SECTOR_SIZE;
    file.seek(SeekFrom::Start(byte_off))
        .map_err(|e| Error::Io {
            path: image_path.to_path_buf(),
            source: e,
        })?;
    file.write_all(&r.bytes).map_err(|e| Error::Io {
        path: image_path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

fn format_partition_in_place(
    image_path: &Path,
    image: &ImageDef,
    rp: &ResolvedPart<'_>,
    partitions: &[ResolvedPartitionEntries<'_>],
    stdout: &mut Vec<u8>,
) -> Result<()> {
    let resolved_entries = &partitions[rp.src_index].entries;

    let start_byte = rp.def.start_lba * SECTOR_SIZE;
    let len_bytes = (rp.end_lba - rp.def.start_lba + 1) * SECTOR_SIZE;

    // Decorate the image name so progress output and any
    // hadris error attributes the failure to the partition.
    let scoped_name = format!("{}::partition({})", image.name, rp.def.name);

    formatter::select_formatter(&rp.def.fs_type, &image.name, &rp.def.name)?.format_in_place(
        image_path,
        start_byte,
        len_bytes,
        &scoped_name,
        resolved_entries,
        stdout,
    )
}

fn describe_raw_source(s: &RawSource) -> String {
    match s {
        RawSource::Crate { name } => format!("crate({name})"),
        RawSource::File { path } => format!("file({path})"),
        RawSource::Bytes { bytes } => format!("bytes(len={})", bytes.len()),
    }
}

/// Pick a sensible image size when the user didn't specify one.
///
/// Strategy: take the explicit `size_mb` if set; otherwise compute the
/// minimum from the input bytes plus some FAT32-overhead headroom.
/// FAT32 needs at least ~33 MiB to be a valid filesystem (one cluster
/// per FAT entry, minimum two FATs, reserved sectors, root cluster) —
/// hadris-fat will refuse to format anything smaller. So we floor at 64
/// MiB to give a comfortable margin.
fn compute_size_bytes(image: &ImageDef, entries: &[ResolvedEntry]) -> Result<u64> {
    if let Some(mb) = image.size_mb {
        // User-specified — trust it. hadris-fat will validate it can
        // host a FAT32 layout and produce a clear error if not.
        return Ok((mb as u64) * 1024 * 1024);
    }

    // Conservative default: round up to the nearest 64 MiB beyond the
    // sum of input sizes, with a 64 MiB minimum. We don't try to nail
    // the smallest valid size because FAT32 cluster overhead makes
    // that calculation hadris's job, not ours.
    let mut total: u64 = 0;
    for e in entries {
        match &e.source_kind {
            StampSource::EspDir { .. } => {
                // Sum the bytes of every regular file in the ESP tree.
                total += sum_dir_bytes(&e.source_path)?;
            }
            _ => {
                let md = std::fs::metadata(&e.source_path).map_err(|err| Error::Io {
                    path: e.source_path.clone(),
                    source: err,
                })?;
                total += md.len();
            }
        }
    }

    const MIN_BYTES: u64 = 64 * 1024 * 1024;
    const ROUND_BYTES: u64 = 64 * 1024 * 1024;
    let rounded = total.div_ceil(ROUND_BYTES) * ROUND_BYTES;
    Ok(rounded.max(MIN_BYTES))
}

fn sum_dir_bytes(dir: &Path) -> Result<u64> {
    let mut total: u64 = 0;
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.map_err(|e| Error::Compile(format!("walkdir: {e}")))?;
        if entry.file_type().is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(total)
}

/// hadris-fat compile-time version, used as a cache-key input so that
/// upgrading the FAT writer transparently invalidates every image
/// (a new hadris release might write the same logical image with a
/// slightly different byte layout — e.g. fixed boot-sector reserved
/// area, free-space hint — and we want determinism across builds, not
/// across hadris versions).
fn hadris_fat_version() -> &'static str {
    // Read from the `hadris-fat` Cargo.toml at compile time. If
    // hadris-fat ever exposes a `pub const VERSION` we should switch
    // to that; for now this env var is set by Cargo and is stable.
    option_env!("DEP_HADRIS_FAT_VERSION").unwrap_or("unknown")
}

#[allow(unused)]
fn _writeln_progress(stdout: &mut Vec<u8>, msg: impl AsRef<str>) {
    let _ = writeln!(stdout, "    {}", msg.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{ArtifactMap, BuildLayout, RustcInfo};
    use gluon_model::{
        BuildModel, ImageEntry, ImageSource, PanicStrategy, ProjectDef, ResolvedConfig,
        ResolvedProfile, TargetDef,
    };
    use std::sync::Arc;

    fn fake_rustc_info() -> RustcInfo {
        RustcInfo {
            rustc_path: PathBuf::from("/usr/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src: None,
            mtime_ns: 0,
        }
    }

    fn make_ctx(tmp: &Path) -> CompileCtx {
        let layout = BuildLayout::new(tmp.join("build"), "imgtest");
        std::fs::create_dir_all(tmp.join("build")).unwrap();
        CompileCtx::new(layout, Arc::new(fake_rustc_info()))
    }

    fn make_resolved(model: &mut BuildModel, project_root: &Path) -> ResolvedConfig {
        let (th, _) = model.targets.insert(
            "x86_64-unknown-none".into(),
            TargetDef {
                name: "x86_64-unknown-none".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: Some(PanicStrategy::Abort),
                span: None,
            },
        );
        ResolvedConfig {
            project: ProjectDef {
                name: "imgtest".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: th,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                qemu_boot_mode: None,
                test_timeout: None,
            },
            options: Default::default(),
            crates: Vec::new(),
            build_dir: project_root.join("build"),
            project_root: project_root.to_path_buf(),
        }
    }

    /// An input whose mtime is at or after the start of the image build
    /// may have been edited while the image was written, so the image
    /// may hold the old content. The next build must rebuild it. A
    /// future mtime stands in for an edit made during the build.
    #[test]
    fn image_input_modified_during_build_rebuilds_next_time() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());
        let host_file = project_root.join("hello.txt");
        std::fs::write(&host_file, b"hello\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&host_file)
            .unwrap()
            .set_modified(later)
            .unwrap();

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);
        let (image_h, _) = model.images.insert(
            "boot".into(),
            ImageDef {
                name: "boot".into(),
                format: Some(ImageFormat::Fat32),
                size_mb: Some(64),
                entries: vec![ImageEntry {
                    source: ImageSource::File("hello.txt".into()),
                    dest_path: "/hello.txt".into(),
                }],
                ..Default::default()
            },
        );
        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };
        let mut stdout = Vec::new();
        let (_, first) = build_image(image_h, &ictx, &mut stdout, &mut Vec::new()).unwrap();
        assert!(!first);
        let (_, second) = build_image(image_h, &ictx, &mut stdout, &mut Vec::new()).unwrap();
        assert!(
            !second,
            "image built from a possibly torn read was served from cache"
        );
    }

    /// Sanity: fat32 image with one file entry produces a non-empty
    /// image whose bytes start with the FAT32 BPB jump (`0xEB`) and
    /// contain the volume label string.
    #[test]
    fn build_image_fat32_writes_valid_bpb_and_label() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());

        // Drop a host file in the project root that the image will pack.
        let host_file = project_root.join("hello.txt");
        std::fs::write(&host_file, b"hello, world!\n").unwrap();

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);

        let (image_h, _) = model.images.insert(
            "boot".into(),
            ImageDef {
                name: "boot".into(),
                format: Some(ImageFormat::Fat32),
                size_mb: Some(64),
                entries: vec![ImageEntry {
                    source: ImageSource::File("hello.txt".into()),
                    dest_path: "/boot/hello.txt".into(),
                }],
                ..Default::default()
            },
        );

        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };

        let mut stdout = Vec::new();
        let (out_path, was_cached) =
            build_image(image_h, &ictx, &mut stdout, &mut Vec::new()).unwrap();
        assert!(!was_cached, "first build must run the formatter");
        assert!(out_path.exists());

        // FAT32 boot sector starts with a JMP (0xEB) or longer JMP (0xE9).
        let bytes = std::fs::read(&out_path).unwrap();
        assert!(bytes.len() == 64 * 1024 * 1024, "size honoured");
        assert!(
            bytes[0] == 0xEB || bytes[0] == 0xE9,
            "first byte must be a JMP opcode (got 0x{:02X})",
            bytes[0]
        );
        // Boot sector signature at bytes 510..512.
        assert_eq!(&bytes[510..512], &[0x55, 0xAA], "boot sector magic");

        // OEM identifier (typically "MSWIN4.1" or similar) at bytes
        // 3..11 is the next mandatory field. We just check it's ASCII
        // — the exact value depends on the formatter and is not part
        // of our contract.
        let oem = &bytes[3..11];
        assert!(
            oem.iter().all(|b| b.is_ascii()),
            "OEM identifier must be ASCII, got: {oem:?}"
        );

        // BPB sector size at offset 11..13 (little-endian) must be
        // a power of two between 512 and 4096. This is the load-bearing
        // structural check: anything that gets past it is structurally
        // a valid FAT volume regardless of the FAT12/16/32 dialect.
        let bps = u16::from_le_bytes([bytes[11], bytes[12]]);
        assert!(
            matches!(bps, 512 | 1024 | 2048 | 4096),
            "bytes-per-sector must be a valid FAT power of two, got: {bps}"
        );
    }

    /// Cache hit: a second build_image with an unchanged stamp returns
    /// `was_cached=true` and does NOT touch the image file's mtime.
    #[test]
    fn build_image_caches_on_unchanged_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());
        let host_file = project_root.join("data.bin");
        std::fs::write(&host_file, b"some bytes").unwrap();

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);
        let (image_h, _) = model.images.insert(
            "img".into(),
            ImageDef {
                name: "img".into(),
                format: Some(ImageFormat::Fat32),
                size_mb: Some(64),
                entries: vec![ImageEntry {
                    source: ImageSource::File("data.bin".into()),
                    dest_path: "/data.bin".into(),
                }],
                ..Default::default()
            },
        );

        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };

        let (out1, cached1) =
            build_image(image_h, &ictx, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert!(!cached1);
        let mtime1 = std::fs::metadata(&out1).unwrap().modified().unwrap();

        // Tiny sleep so the mtime would tick if a write happened.
        std::thread::sleep(std::time::Duration::from_millis(10));

        let (out2, cached2) =
            build_image(image_h, &ictx, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert_eq!(out1, out2);
        assert!(cached2, "second build must hit the stamp cache");
        let mtime2 = std::fs::metadata(&out2).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "cached path must not rewrite the image");
    }

    /// Cache miss: a touch on the source file invalidates the stamp
    /// and the image is rewritten.
    #[test]
    fn build_image_invalidates_when_source_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());
        let host_file = project_root.join("data.bin");
        std::fs::write(&host_file, b"v1").unwrap();

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);
        let (image_h, _) = model.images.insert(
            "img".into(),
            ImageDef {
                name: "img".into(),
                format: Some(ImageFormat::Fat32),
                size_mb: Some(64),
                entries: vec![ImageEntry {
                    source: ImageSource::File("data.bin".into()),
                    dest_path: "/data.bin".into(),
                }],
                ..Default::default()
            },
        );

        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };

        let (_, cached1) = build_image(image_h, &ictx, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert!(!cached1);

        // Wait so the mtime tick is observable, then rewrite the source.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&host_file, b"v2-different-bytes").unwrap();

        let (_, cached2) = build_image(image_h, &ictx, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert!(!cached2, "source change must bust the stamp cache");
    }

    /// `format("raw")` is a Phase 2 explicit error: we want users to
    /// know the feature is coming, not silently produce an empty file.
    #[test]
    fn build_image_raw_format_errors_with_helpful_message() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);
        let (image_h, _) = model.images.insert(
            "raw".into(),
            ImageDef {
                name: "raw".into(),
                format: Some(ImageFormat::Raw),
                ..Default::default()
            },
        );

        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };

        let err = build_image(image_h, &ictx, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("partition_table") && msg.contains("write_"),
            "raw error must point at the partition_table + write_*_at primitives, got: {msg}"
        );
    }

    /// End-to-end: an MBR-partitioned disk with one inline-bytes raw
    /// write at LBA 0 (the boot sector code) and one FAT32 ESP starting
    /// at LBA 2048. Verify:
    ///   1. The boot-sector bytes survived in 0..446.
    ///   2. The MBR entry at 446 has the expected type byte (0xEF).
    ///   3. The 0x55AA signature is at 510..512.
    ///   4. The byte range starting at LBA 2048 is a valid FAT32 BPB.
    #[test]
    fn build_image_mbr_with_raw_write_and_fat32_partition() {
        use gluon_model::{PartitionDef, PartitionTable, RawSource, RawWrite};

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());

        // A boot-sector blob that fills bytes 0..16 with a recognisable
        // signature ("BOOTSECTORCODE!!"). Bytes 16..446 stay zero from
        // our zero-fill; bytes 446..512 will be overwritten by the MBR
        // pass.
        let mut boot_code = vec![0u8; 446];
        boot_code[..16].copy_from_slice(b"BOOTSECTORCODE!!");

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);
        let (image_h, _) = model.images.insert(
            "disk".into(),
            ImageDef {
                name: "disk".into(),
                size_mb: Some(64),
                partition_table: Some(PartitionTable::Mbr),
                raw_writes: vec![RawWrite {
                    lba: 0,
                    source: RawSource::Bytes { bytes: boot_code },
                    max_sectors: Some(1),
                    span: None,
                }],
                partitions: vec![PartitionDef {
                    name: "esp".into(),
                    start_lba: 2048,
                    end_lba: None, // fill rest
                    fs_type: "fat32".into(),
                    partition_type: 0xEF,
                    bootable: true,
                    entries: Vec::new(),
                    span: None,
                }],
                ..Default::default()
            },
        );

        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };

        let mut stdout = Vec::new();
        let (out_path, _) = build_image(image_h, &ictx, &mut stdout, &mut Vec::new()).unwrap();
        let bytes = std::fs::read(&out_path).unwrap();
        assert_eq!(bytes.len() as u64, 64 * 1024 * 1024);

        // 1. Boot-sector code preserved in 0..16.
        assert_eq!(&bytes[..16], b"BOOTSECTORCODE!!");

        // 2. MBR partition entry at 446..462: type=0xEF, start=2048,
        //    bootable=0x80.
        let entry = &bytes[446..462];
        assert_eq!(entry[0], 0x80, "bootable flag set");
        assert_eq!(entry[4], 0xEF, "ESP partition type byte");
        let start = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
        assert_eq!(start, 2048, "start LBA");

        // 3. MBR signature.
        assert_eq!(&bytes[510..512], &[0x55, 0xAA]);

        // 4. FAT32 BPB at byte offset 2048*512: jump opcode + valid
        //    bytes-per-sector field.
        let bpb_off = 2048 * 512;
        assert!(
            bytes[bpb_off] == 0xEB || bytes[bpb_off] == 0xE9,
            "partition starts with FAT JMP opcode (got 0x{:02X})",
            bytes[bpb_off]
        );
        let bps = u16::from_le_bytes([bytes[bpb_off + 11], bytes[bpb_off + 12]]);
        assert!(
            matches!(bps, 512 | 1024 | 2048 | 4096),
            "partition BPB bytes-per-sector valid, got: {bps}"
        );
    }

    /// Validation: a raw write that overlaps a partition must error
    /// with a message identifying both ranges.
    #[test]
    fn build_image_rejects_overlapping_raw_write_and_partition() {
        use gluon_model::{PartitionDef, PartitionTable, RawSource, RawWrite};

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);
        let (image_h, _) = model.images.insert(
            "bad".into(),
            ImageDef {
                name: "bad".into(),
                size_mb: Some(64),
                partition_table: Some(PartitionTable::Mbr),
                raw_writes: vec![RawWrite {
                    // 32 KiB blob = 64 sectors, but starts at LBA 2050
                    // which is inside the partition.
                    lba: 2050,
                    source: RawSource::Bytes {
                        bytes: vec![0xAB; 32 * 1024],
                    },
                    max_sectors: None,
                    span: None,
                }],
                partitions: vec![PartitionDef {
                    name: "esp".into(),
                    start_lba: 2048,
                    end_lba: Some(4095),
                    fs_type: "fat32".into(),
                    partition_type: 0xEF,
                    bootable: false,
                    entries: Vec::new(),
                    span: None,
                }],
                ..Default::default()
            },
        );

        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };
        let err = build_image(image_h, &ictx, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("overlaps") && msg.contains("partition('esp')"),
            "expected an overlap diagnostic naming the partition, got: {msg}"
        );
    }

    /// Image freshness lives in the fingerprint record `image:<name>`,
    /// not in a sidecar file. Building an image must NOT leave a
    /// `<image>.stamp.json` next to the output.
    #[test]
    fn build_image_writes_no_stamp_json_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());

        let host_file = project_root.join("hello.txt");
        std::fs::write(&host_file, b"hello, world!\n").unwrap();

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);
        let (image_h, _) = model.images.insert(
            "boot".into(),
            ImageDef {
                name: "boot".into(),
                format: Some(ImageFormat::Fat32),
                size_mb: Some(64),
                entries: vec![ImageEntry {
                    source: ImageSource::File("hello.txt".into()),
                    dest_path: "/boot/hello.txt".into(),
                }],
                ..Default::default()
            },
        );

        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };

        let mut stdout = Vec::<u8>::new();
        let (image_path, _was_cached) =
            build_image(image_h, &ictx, &mut stdout, &mut Vec::new()).unwrap();

        let stamp_path = image_path.with_extension("stamp.json");
        assert!(
            !stamp_path.exists(),
            "build_image must not write {}; that data lives in the cache manifest now",
            stamp_path.display()
        );
    }

    /// Validation: max_sectors enforces a hard upper bound on a
    /// raw_write's payload size.
    #[test]
    fn build_image_rejects_raw_write_exceeding_max_sectors() {
        use gluon_model::{PartitionTable, RawSource, RawWrite};

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let ctx = make_ctx(tmp.path());

        let mut model = BuildModel::default();
        let resolved = make_resolved(&mut model, &project_root);
        let (image_h, _) = model.images.insert(
            "bad".into(),
            ImageDef {
                name: "bad".into(),
                size_mb: Some(64),
                partition_table: Some(PartitionTable::Mbr),
                // 1 KiB = 2 sectors, capped at 1.
                raw_writes: vec![RawWrite {
                    lba: 1,
                    source: RawSource::Bytes {
                        bytes: vec![0xCD; 1024],
                    },
                    max_sectors: Some(1),
                    span: None,
                }],
                ..Default::default()
            },
        );

        let artifacts = ArtifactMap::new();
        let ictx = ImageCtx {
            ctx: &ctx,
            model: &model,
            resolved: &resolved,
            artifacts: &artifacts,
        };

        // We need at least one partition or raw_writes for the build
        // to attempt anything; raw_writes alone is fine.
        let err = build_image(image_h, &ictx, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("max_sectors") && msg.contains("2 sectors"),
            "expected max_sectors diagnostic with sector count, got: {msg}"
        );
    }
}
