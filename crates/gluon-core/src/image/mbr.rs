//! MBR (Master Boot Record) partition table emission.
//!
//! Layout of sector 0 (512 bytes):
//!
//! | Offset      | Size | Content                                      |
//! |-------------|------|----------------------------------------------|
//! | `0..446`    |  446 | Boot code (preserved if a `RawWrite` at LBA 0 supplied it; else zeroed). |
//! | `446..510`  |   64 | 4 partition entries × 16 bytes.              |
//! | `510..512`  |    2 | `0x55 0xAA` boot-sector signature.           |
//!
//! We emit at most 4 primary partitions (MBR's hard cap). Extended /
//! logical partitions are out of scope — bare-metal targets that need
//! more than 4 should use GPT instead, which lands in a future phase.
//!
//! Each partition entry is:
//!
//! | Offset | Size | Field                                            |
//! |--------|------|--------------------------------------------------|
//! |   0    |   1  | Active/bootable flag (0x80 = bootable, 0x00 = not). |
//! |   1..4 |   3  | CHS address of first sector (legacy).            |
//! |   4    |   1  | Partition type byte (e.g. 0xEF for ESP).         |
//! |   5..8 |   3  | CHS address of last sector (legacy).             |
//! |   8..12|   4  | LBA of first sector (u32 LE).                    |
//! |  12..16|   4  | Number of sectors in the partition (u32 LE).     |
//!
//! Modern firmwares (UEFI's CSM, SeaBIOS) use the LBA fields; we emit
//! the canonical CHS placeholder `0xFE 0xFF 0xFF` for both addresses,
//! which signals "this entry uses LBA, ignore CHS." Some very old BIOSes
//! choke on this — those aren't a target this build system supports.

use crate::error::{Error, Result};
use gluon_model::PartitionDef;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

/// 512-byte sector size assumed throughout the image builder. FAT32
/// supports other values, but every BIOS/UEFI disk in practice (and
/// every value hadris-fat formats with) uses 512.
pub(super) const SECTOR_SIZE: u64 = 512;

const MBR_PARTITION_TABLE_OFFSET: u64 = 446;
const MBR_SIGNATURE_OFFSET: u64 = 510;
const MBR_MAX_PARTITIONS: usize = 4;

/// Resolved partition: the user wrote `end_lba: None` ("fill to end"),
/// and we've replaced that with the concrete last-sector value once the
/// total image size is known.
pub(super) struct ResolvedPartition<'a> {
    pub def: &'a PartitionDef,
    pub end_lba: u64,
}

/// Write the MBR partition table at sector 0 of `image_path`. The
/// boot-code area (bytes 0..446) is left untouched — earlier `RawWrite`
/// passes are responsible for filling it. We only stamp the 64-byte
/// table region and the trailing signature.
///
/// `partitions` carries the resolved end LBAs (None replaced with the
/// concrete value). Callers must validate overlaps and 4-partition cap
/// before invoking; this function trusts its inputs.
pub(super) fn write_mbr(
    image_path: &Path,
    image_name: &str,
    partitions: &[ResolvedPartition<'_>],
) -> Result<()> {
    if partitions.len() > MBR_MAX_PARTITIONS {
        // Should be caught by validation upstream — defensive belt &
        // braces. The diagnostic spans the whole image, not a specific
        // partition, because the user has too many in aggregate.
        return Err(Error::Compile(format!(
            "image('{}'): MBR allows at most {MBR_MAX_PARTITIONS} primary partitions, got {}",
            image_name,
            partitions.len()
        )));
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image_path)
        .map_err(|e| Error::Io {
            path: image_path.to_path_buf(),
            source: e,
        })?;

    // Write each entry into the 64-byte table area. Unused entry slots
    // are zeroed — we explicitly write zeros for those so a previous
    // (failed) build's bytes can't bleed through.
    let mut table = [0u8; 64];
    for (i, p) in partitions.iter().enumerate() {
        let entry = encode_partition_entry(p, image_name)?;
        let off = i * 16;
        table[off..off + 16].copy_from_slice(&entry);
    }

    file.seek(SeekFrom::Start(MBR_PARTITION_TABLE_OFFSET))
        .map_err(|e| Error::Io {
            path: image_path.to_path_buf(),
            source: e,
        })?;
    file.write_all(&table).map_err(|e| Error::Io {
        path: image_path.to_path_buf(),
        source: e,
    })?;

    // Boot-sector signature. Always 0x55 0xAA. Required by every BIOS
    // that performs the legacy bootstrap; UEFI doesn't read this byte
    // pair but doesn't object to it either.
    file.seek(SeekFrom::Start(MBR_SIGNATURE_OFFSET))
        .map_err(|e| Error::Io {
            path: image_path.to_path_buf(),
            source: e,
        })?;
    file.write_all(&[0x55, 0xAA]).map_err(|e| Error::Io {
        path: image_path.to_path_buf(),
        source: e,
    })?;

    Ok(())
}

fn encode_partition_entry(p: &ResolvedPartition<'_>, image_name: &str) -> Result<[u8; 16]> {
    let mut out = [0u8; 16];

    // Active flag.
    out[0] = if p.def.bootable { 0x80 } else { 0x00 };

    // CHS first / last: emit the canonical "use LBA, CHS unused"
    // placeholder. Some legacy BIOSes choked on 0/0/0; 0xFE/0xFF/0xFF
    // is the widely-accepted sentinel.
    out[1] = 0xFE;
    out[2] = 0xFF;
    out[3] = 0xFF;

    // Partition type.
    out[4] = p.def.partition_type;

    out[5] = 0xFE;
    out[6] = 0xFF;
    out[7] = 0xFF;

    // LBA fields. MBR's 32-bit-LBA fields cap at ~2 TiB on a 512-byte
    // sector disk. Bare-metal kernel images won't hit that; we still
    // surface a clear error if they do.
    let start: u32 = p.def.start_lba.try_into().map_err(|_| {
        Error::Compile(format!(
            "image('{}') partition('{}'): start_lba {} exceeds MBR's 32-bit limit (use GPT for >2 TiB disks)",
            image_name, p.def.name, p.def.start_lba
        ))
    })?;
    let sector_count_inclusive = p.end_lba.saturating_sub(p.def.start_lba) + 1;
    let count: u32 = sector_count_inclusive.try_into().map_err(|_| {
        Error::Compile(format!(
            "image('{}') partition('{}'): sector count {} exceeds MBR's 32-bit limit",
            image_name, p.def.name, sector_count_inclusive
        ))
    })?;

    out[8..12].copy_from_slice(&start.to_le_bytes());
    out[12..16].copy_from_slice(&count.to_le_bytes());

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_part(name: &str, start: u64, end: u64, ptype: u8, bootable: bool) -> PartitionDef {
        PartitionDef {
            name: name.into(),
            start_lba: start,
            end_lba: Some(end),
            fs_type: "fat32".into(),
            partition_type: ptype,
            bootable,
            entries: Vec::new(),
            span: None,
        }
    }

    /// Round-trip: a single ESP partition produces:
    ///   - 0x80 active flag
    ///   - 0xEF type
    ///   - LBA fields matching start / count
    ///   - 0x55AA signature
    /// And the boot-code area is left untouched (we pre-fill with 0xCC).
    #[test]
    fn write_mbr_emits_canonical_partition_entry_and_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("disk.img");

        // Make a 1 MiB image pre-filled with 0xCC so we can prove the
        // boot-code region (bytes 0..446) isn't touched.
        let prefill = vec![0xCCu8; 1024 * 1024];
        fs::write(&path, &prefill).unwrap();

        let pdef = make_part("esp", 2048, 4095, 0xEF, true);
        let resolved = vec![ResolvedPartition {
            def: &pdef,
            end_lba: 4095,
        }];

        write_mbr(&path, "test", &resolved).unwrap();

        let bytes = fs::read(&path).unwrap();

        // Boot-code region preserved.
        assert!(
            bytes[..446].iter().all(|&b| b == 0xCC),
            "boot-code region must not be touched"
        );

        // First partition entry at 446..462.
        let entry = &bytes[446..462];
        assert_eq!(entry[0], 0x80, "active flag set");
        assert_eq!(entry[4], 0xEF, "partition type byte");
        assert_eq!(
            u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]),
            2048,
            "start LBA"
        );
        assert_eq!(
            u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]),
            4095 - 2048 + 1,
            "sector count (inclusive)"
        );

        // Remaining 3 entries are zeroed.
        assert!(
            bytes[462..510].iter().all(|&b| b == 0),
            "unused partition slots must be zero"
        );

        // Signature.
        assert_eq!(&bytes[510..512], &[0x55, 0xAA]);
    }

    #[test]
    fn write_mbr_rejects_more_than_four_partitions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("disk.img");
        fs::write(&path, vec![0u8; 1024]).unwrap();

        let pdefs: Vec<PartitionDef> = (0..5)
            .map(|i| make_part(&format!("p{i}"), 1 + i * 10, 9 + i * 10, 0x83, false))
            .collect();
        let resolved: Vec<ResolvedPartition> = pdefs
            .iter()
            .map(|p| ResolvedPartition {
                def: p,
                end_lba: p.end_lba.unwrap(),
            })
            .collect();

        let err = write_mbr(&path, "test", &resolved).unwrap_err();
        assert!(err.to_string().contains("at most 4"));
    }
}
