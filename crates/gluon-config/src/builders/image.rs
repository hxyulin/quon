//! Chainable builder for [`ImageDef`] entries.

use crate::builders::BuilderState;
use crate::builders::partition::PartitionBuilder;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{
    Handle, ImageDef, ImageEntry, ImageFormat, ImageSource, PartitionDef, PartitionTable,
    RawSource, RawWrite,
};

/// Chainable builder returned by [`Config::image`].
pub struct ImageBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<ImageDef>>,
}

impl<'cfg> ImageBuilder<'cfg> {
    /// Apply `f` to the [`ImageDef`] if this builder is `Valid`.
    fn modify<F: FnOnce(&mut ImageDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(img) = self.cfg.raw.images.get_mut(*handle) {
                f(img);
            }
        }
    }

    /// Set the image filesystem flavour. The user-facing API stays
    /// `&str` so call sites read like the old form
    /// (`.format("fat32")`); we parse into [`ImageFormat`] here so the
    /// raw and resolved models both carry the typed enum.
    ///
    /// Unknown values are surfaced as a [`ConfigError`] diagnostic and
    /// the field is left unset — the alternative (silently storing a
    /// `Custom(String)` escape hatch like `BootloaderKind`) was
    /// rejected because the format set is closed: the dispatch site in
    /// `gluon-core::image` can't *do* anything with an unrecognised
    /// flavour, so the failure should be visible at config time, not
    /// at image-build time.
    pub fn format(mut self, name: &str) -> Self {
        match ImageFormat::parse(name) {
            Some(f) => {
                self.modify(|img| img.format = Some(f));
            }
            None => {
                // Don't gate on `is_duplicate`: a bad format string is
                // worth surfacing even when the image itself is a dup
                // — the user wrote it, they should see the typo.
                self.cfg.diagnostics.push(ConfigError::new(format!(
                    "image format '{name}' is not recognised \
                     (expected fat12|fat16|fat32|raw|iso9660)"
                )));
            }
        }
        self
    }

    /// Set the image size in MiB.
    pub fn size_mb(mut self, mb: u32) -> Self {
        self.modify(|img| img.size_mb = Some(mb));
        self
    }

    /// Append a crate-artifact entry to the image's flat filesystem.
    pub fn add_entry_crate(mut self, crate_name: &str, dest_path: &str) -> Self {
        let entry = ImageEntry {
            source: ImageSource::Crate(crate_name.into()),
            dest_path: dest_path.into(),
        };
        self.modify(|img| img.entries.push(entry));
        self
    }

    /// Append a host-file entry to the image's flat filesystem.
    pub fn add_entry_file(mut self, path: &str, dest_path: &str) -> Self {
        let entry = ImageEntry {
            source: ImageSource::File(path.into()),
            dest_path: dest_path.into(),
        };
        self.modify(|img| img.entries.push(entry));
        self
    }

    /// Append a pinned download (declared with `cfg.download(...)` or
    /// `cfg.limine(...)`) to the image's flat filesystem.
    pub fn add_entry_download(mut self, name: &str, dest_path: &str) -> Self {
        let entry = ImageEntry {
            source: ImageSource::Download(name.into()),
            dest_path: dest_path.into(),
        };
        self.modify(|img| img.entries.push(entry));
        self
    }

    /// Append an assembled ESP entry to the image's flat filesystem.
    pub fn add_entry_esp(mut self, esp_name: &str, dest_path: &str) -> Self {
        let entry = ImageEntry {
            source: ImageSource::Esp(esp_name.into()),
            dest_path: dest_path.into(),
        };
        self.modify(|img| img.entries.push(entry));
        self
    }

    /// Set the partition table type. Switches the image into
    /// partitioned-disk mode.
    pub fn partition_table(mut self, table: PartitionTable) -> Self {
        self.modify(|img| img.partition_table = Some(table));
        self
    }

    /// Append a raw crate-artifact write at a specific LBA.
    pub fn raw_write_crate(mut self, lba: u64, crate_name: &str, max_sectors: Option<u64>) -> Self {
        let rw = RawWrite {
            lba,
            source: RawSource::Crate {
                name: crate_name.into(),
            },
            max_sectors,
            span: None,
        };
        self.modify(|img| img.raw_writes.push(rw));
        self
    }

    /// Append a raw host-file write at a specific LBA.
    pub fn raw_write_file(mut self, lba: u64, path: &str, max_sectors: Option<u64>) -> Self {
        let rw = RawWrite {
            lba,
            source: RawSource::File { path: path.into() },
            max_sectors,
            span: None,
        };
        self.modify(|img| img.raw_writes.push(rw));
        self
    }

    /// Append an inline-bytes write at a specific LBA.
    pub fn raw_write_bytes(mut self, lba: u64, bytes: Vec<u8>, max_sectors: Option<u64>) -> Self {
        let rw = RawWrite {
            lba,
            source: RawSource::Bytes { bytes },
            max_sectors,
            span: None,
        };
        self.modify(|img| img.raw_writes.push(rw));
        self
    }

    /// Open a sub-builder for a partition on this image.
    ///
    /// Returns a [`PartitionBuilder`] that lets you configure the
    /// individual partition. Call `.end()` on it to get the
    /// [`ImageBuilder`] back so you can continue chaining at the disk
    /// level.
    ///
    /// # Partition table requirement
    ///
    /// The engine does not emit a diagnostic at config-definition time
    /// for calling `.partition()` without first calling
    /// `.partition_table(...)`. The inconsistency is caught at
    /// image-build time. This builder mirrors that behaviour: the
    /// partition is recorded unconditionally and no diagnostic is
    /// pushed here.
    pub fn partition(self, name: &str, start_lba: u64, fs_type: &str) -> PartitionBuilder<'cfg> {
        // If already a duplicate, propagate without touching the model.
        if self.state.is_duplicate() {
            let state = BuilderState::Duplicate {
                name: name.into(),
                span: None,
            };
            // Placeholder handle — never resolved during a successful path:
            // `PartitionBuilder::modify` short-circuits on `is_duplicate`, and
            // `end()` reads via `arena.get(...).map(name).unwrap_or_default()`,
            // so a stale or out-of-bounds value is benign (resulting builder
            // stays Duplicate with an empty name; chained methods no-op).
            let image_handle: Handle<ImageDef> = Handle::new(0);
            return PartitionBuilder {
                cfg: self.cfg,
                image_handle,
                partition_index: usize::MAX,
                state,
            };
        }

        let handle = match &self.state {
            BuilderState::Valid { state: h, .. } => *h,
            BuilderState::Duplicate { .. } => unreachable!("handled above"),
        };

        // Append the new partition and note its index.
        let partition_index = {
            if let Some(img) = self.cfg.raw.images.get_mut(handle) {
                let idx = img.partitions.len();
                img.partitions.push(PartitionDef {
                    name: name.into(),
                    start_lba,
                    end_lba: None,
                    fs_type: fs_type.into(),
                    // Default to 0x83 (Linux) — mirrors the engine default.
                    partition_type: 0x83,
                    bootable: false,
                    entries: Vec::new(),
                    span: None,
                });
                idx
            } else {
                // Handle no longer valid — push a diagnostic and short-circuit.
                self.cfg.diagnostics.push(ConfigError::new(format!(
                    "image(\"{}\").partition: internal error — image handle is stale",
                    self.state.name()
                )));
                let state = BuilderState::Duplicate {
                    name: name.into(),
                    span: None,
                };
                return PartitionBuilder {
                    cfg: self.cfg,
                    image_handle: handle,
                    partition_index: usize::MAX,
                    state,
                };
            }
        };

        let state = BuilderState::Valid {
            state: (),
            name: name.into(),
            span: None,
        };
        PartitionBuilder {
            cfg: self.cfg,
            image_handle: handle,
            partition_index,
            state,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use gluon_model::{ImageFormat, ImageSource, PartitionTable};

    #[test]
    fn image_with_entries_chain() {
        let mut cfg = Config::new();
        cfg.image("disk")
            .format("fat32")
            .size_mb(64)
            .add_entry_crate("kernel", "boot/kernel.bin")
            .add_entry_file("README.txt", "docs/readme.txt");

        let h = cfg.raw.images.lookup("disk").unwrap();
        let img = cfg.raw.images.get(h).unwrap();
        assert_eq!(img.format, Some(ImageFormat::Fat32));
        assert_eq!(img.size_mb, Some(64));
        assert_eq!(img.entries.len(), 2);
        match &img.entries[0].source {
            ImageSource::Crate(name) => assert_eq!(name, "kernel"),
            other => panic!("expected Crate, got {other:?}"),
        }
        assert_eq!(img.entries[0].dest_path, "boot/kernel.bin");
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_image_short_circuits_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.image("disk").size_mb(64);
        cfg.image("disk").size_mb(128);
        let h = cfg.raw.images.lookup("disk").unwrap();
        let img = cfg.raw.images.get(h).unwrap();
        assert_eq!(img.size_mb, Some(64), "first definition preserved");
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("disk"));
    }

    #[test]
    fn partitioned_disk_full_chain() {
        let mut cfg = Config::new();
        cfg.image("hybrid")
            .partition_table(PartitionTable::Mbr)
            .raw_write_crate(0, "boot_sector", Some(1))
            .partition("esp", 2048, "fat32")
            .partition_type(0xEF)
            .bootable()
            .add("bootloader", "EFI/BOOT/BOOTX64.EFI")
            .end()
            .partition("root", 32_768, "fat32")
            .end_lba(2_097_151)
            .partition_type(0x0C)
            .end()
            .size_mb(1024);

        let h = cfg.raw.images.lookup("hybrid").unwrap();
        let img = cfg.raw.images.get(h).unwrap();
        assert_eq!(img.partition_table, Some(PartitionTable::Mbr));
        assert_eq!(img.size_mb, Some(1024));
        assert_eq!(img.raw_writes.len(), 1);
        assert_eq!(img.raw_writes[0].lba, 0);
        assert_eq!(img.raw_writes[0].max_sectors, Some(1));
        assert_eq!(img.partitions.len(), 2);
        assert_eq!(img.partitions[0].name, "esp");
        assert_eq!(img.partitions[0].start_lba, 2048);
        assert_eq!(img.partitions[0].partition_type, 0xEF);
        assert!(img.partitions[0].bootable);
        assert_eq!(img.partitions[0].entries.len(), 1);
        assert_eq!(
            img.partitions[0].entries[0].dest_path,
            "EFI/BOOT/BOOTX64.EFI"
        );
        assert_eq!(img.partitions[1].name, "root");
        assert_eq!(img.partitions[1].end_lba, Some(2_097_151));
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn partition_without_partition_table_records_partition_no_diagnostic() {
        // The engine does NOT emit a diagnostic when .partition() is called
        // without a prior .partition_table() call — it simply records the
        // partition. Validation at image-build time catches the inconsistency.
        // This builder mirrors that behaviour.
        let mut cfg = Config::new();
        cfg.image("flat").partition("oops", 0, "fat32").end();

        let h = cfg.raw.images.lookup("flat").unwrap();
        let img = cfg.raw.images.get(h).unwrap();
        // Partition was recorded.
        assert_eq!(img.partitions.len(), 1);
        assert_eq!(img.partitions[0].name, "oops");
        // No diagnostic at definition time.
        assert!(cfg.diagnostics.is_empty());
    }
}
