//! Chainable builder for a [`PartitionDef`] inside an [`ImageDef`].
//!
//! Returned by [`ImageBuilder::partition`]. Call [`.end()`](PartitionBuilder::end)
//! to return ownership of the parent [`ImageBuilder`] so you can keep chaining
//! at the disk level.

use crate::builders::BuilderState;
use crate::builders::image::ImageBuilder;
use crate::config::Config;
use gluon_model::{Handle, ImageDef, ImageEntry, ImageSource, PartitionDef};

/// Chainable builder for a single [`PartitionDef`] on an image.
///
/// Obtained from [`ImageBuilder::partition`]. All mutation methods short-circuit
/// when the builder is in the `Duplicate` state, preserving the first
/// definition.
pub struct PartitionBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    /// Handle to the parent `ImageDef` in the arena. Stable for the duration
    /// of the builder chain because partitions are only ever pushed (never
    /// reordered or removed) during config evaluation.
    pub(crate) image_handle: Handle<ImageDef>,
    /// Index into `ImageDef::partitions`. Stable for the same reason.
    pub(crate) partition_index: usize,
    pub(crate) state: BuilderState<()>,
}

impl<'cfg> PartitionBuilder<'cfg> {
    /// Apply `f` to the targeted [`PartitionDef`] if this builder is `Valid`
    /// and the image/partition are reachable.
    fn modify<F: FnOnce(&mut PartitionDef)>(&mut self, f: F) {
        if !self.state.is_duplicate() {
            if let Some(img) = self.cfg.raw.images.get_mut(self.image_handle) {
                if let Some(p) = img.partitions.get_mut(self.partition_index) {
                    f(p);
                }
            }
        }
    }

    /// Set the inclusive end LBA for this partition. `None` (the default)
    /// means "extend to the last available sector of the image".
    pub fn end_lba(mut self, lba: u64) -> Self {
        self.modify(|p| p.end_lba = Some(lba));
        self
    }

    /// Set the MBR partition type byte.
    ///
    /// Common values: `0x83` (Linux, the default), `0xEF` (EFI System
    /// Partition), `0x0C` (FAT32 with LBA addressing).
    pub fn partition_type(mut self, byte: u8) -> Self {
        self.modify(|p| p.partition_type = byte);
        self
    }

    /// Mark this partition as active/bootable in the MBR.
    pub fn bootable(mut self) -> Self {
        self.modify(|p| p.bootable = true);
        self
    }

    /// Append a crate-artifact entry to this partition's filesystem.
    pub fn add(mut self, crate_name: &str, dest_path: &str) -> Self {
        let entry = ImageEntry {
            source: ImageSource::Crate(crate_name.into()),
            dest_path: dest_path.into(),
        };
        self.modify(|p| p.entries.push(entry));
        self
    }

    /// Close the partition sub-builder and return the parent [`ImageBuilder`]
    /// for the same image, so the caller can continue chaining at the disk
    /// level (declare another partition, set `size_mb`, add raw writes, etc.).
    pub fn end(self) -> ImageBuilder<'cfg> {
        // Recover the image's name so ImageBuilder can reconstruct its state.
        let name = self
            .cfg
            .raw
            .images
            .get(self.image_handle)
            .map(|i| i.name.clone())
            .unwrap_or_default();

        let state = if self.state.is_duplicate() {
            BuilderState::Duplicate { name, span: None }
        } else {
            BuilderState::Valid {
                state: self.image_handle,
                name,
                span: None,
            }
        };

        ImageBuilder {
            cfg: self.cfg,
            state,
        }
    }
}
