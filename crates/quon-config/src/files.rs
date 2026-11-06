//! Files Quon fetches or assembles: downloads, ESPs, disk images and
//! build steps.

use crate::artifact::{Artifact, Source};
use crate::quon::{Declare, Quon};
use quon_model::{ImageFormat, PartitionTable};
use std::collections::BTreeMap;

/// Where Limine publishes prebuilt binaries: one `v<version>-binary`
/// tag per release. Tags, unlike the `v<major>.x-binary` branches, do
/// not move, and the pinned hash catches a tag that does.
const LIMINE_BINARY_URL: &str = "https://raw.githubusercontent.com/Limine-Bootloader/Limine";

/// A file fetched from a URL and checked against its SHA-256.
///
/// Quon fetches it with `curl` the first time something needs it and
/// caches it under `build/downloads/`. An empty hash makes the build
/// fail and print the hash of the fetched file, to pin once you have
/// checked where it came from.
#[derive(Debug, Clone)]
#[must_use = "a download does nothing until it is passed to `q.add`"]
pub struct Download {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) sha256: String,
}

impl Download {
    pub fn new(name: impl Into<String>, url: impl Into<String>, sha256: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            sha256: sha256.into().trim().to_ascii_lowercase(),
        }
    }

    /// `file` from a Limine release, such as `BOOTX64.EFI`, named
    /// `limine/<file>`. To upgrade, change the version and the hash.
    ///
    /// ```
    /// # use quon_config::prelude::*;
    /// # let mut q = Quon::new();
    /// let efi = q.add(Download::limine("11.4.1", "BOOTX64.EFI", ""));
    /// q.add(Esp::new("boot").add(&efi, "EFI/BOOT/BOOTX64.EFI"));
    /// ```
    pub fn limine(version: &str, file: &str, sha256: impl Into<String>) -> Self {
        let version = version.trim_start_matches('v');
        Self::new(
            format!("limine/{file}"),
            format!("{LIMINE_BINARY_URL}/v{version}-binary/{file}"),
            sha256,
        )
    }
}

impl Declare for Download {
    type Handle = Artifact;
    fn declare(self, q: &mut Quon) -> Artifact {
        q.add_download(self)
    }
}

/// An EFI system partition, assembled as a directory. A UEFI QEMU
/// boot mounts it when it is the project's only ESP.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// # let boot = q.add(Package::new("bootloader"));
/// q.add(Esp::new("boot").add(&boot, "EFI/BOOT/BOOTX64.EFI"));
/// ```
#[derive(Debug, Clone)]
#[must_use = "an ESP does nothing until it is passed to `q.add`"]
pub struct Esp {
    pub(crate) name: String,
    pub(crate) entries: Vec<(Source, String)>,
}

impl Esp {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entries: Vec::new(),
        }
    }

    /// Copy `source` to `dest`, a path relative to the ESP root.
    pub fn add(mut self, source: impl Into<Source>, dest: impl Into<String>) -> Self {
        self.entries.push((source.into(), dest.into()));
        self
    }
}

impl Declare for Esp {
    type Handle = Artifact;
    fn declare(self, q: &mut Quon) -> Artifact {
        q.add_esp(self)
    }
}

/// A disk image: a flat filesystem, or a partitioned disk with raw
/// writes at fixed sectors.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// # let stage1 = q.add(Crate::raw_binary("stage1", "crates/stage1"));
/// # let kernel = q.add(Package::new("kernel"));
/// q.add(
///     Image::new("disk")
///         .size_mb(64)
///         .partition_table(PartitionTable::Mbr)
///         .raw_write(0, &stage1, Some(1))
///         .partition(Partition::new("boot", 2048, "fat32").bootable().add(&kernel, "kernel")),
/// );
/// ```
#[derive(Debug, Clone)]
#[must_use = "an image does nothing until it is passed to `q.add`"]
pub struct Image {
    pub(crate) name: String,
    pub(crate) format: Option<ImageFormat>,
    pub(crate) size_mb: Option<u32>,
    pub(crate) entries: Vec<(Source, String)>,
    pub(crate) partition_table: Option<PartitionTable>,
    pub(crate) partitions: Vec<Partition>,
    pub(crate) raw_writes: Vec<(u64, Source, Option<u64>)>,
}

impl Image {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            format: None,
            size_mb: None,
            entries: Vec::new(),
            partition_table: None,
            partitions: Vec::new(),
            raw_writes: Vec::new(),
        }
    }

    pub fn format(mut self, format: ImageFormat) -> Self {
        self.format = Some(format);
        self
    }

    pub fn size_mb(mut self, mb: u32) -> Self {
        self.size_mb = Some(mb);
        self
    }

    /// Copy `source` to `dest` in the image's flat filesystem.
    pub fn add(mut self, source: impl Into<Source>, dest: impl Into<String>) -> Self {
        self.entries.push((source.into(), dest.into()));
        self
    }

    /// Make the image a partitioned disk.
    pub fn partition_table(mut self, table: PartitionTable) -> Self {
        self.partition_table = Some(table);
        self
    }

    pub fn partition(mut self, partition: Partition) -> Self {
        self.partitions.push(partition);
        self
    }

    /// Write `source` at sector `lba`, failing the build if it is
    /// longer than `max_sectors`.
    pub fn raw_write(
        mut self,
        lba: u64,
        source: impl Into<Source>,
        max_sectors: Option<u64>,
    ) -> Self {
        self.raw_writes.push((lba, source.into(), max_sectors));
        self
    }
}

impl Declare for Image {
    type Handle = Artifact;
    fn declare(self, q: &mut Quon) -> Artifact {
        q.add_image(self)
    }
}

/// A partition of an [`Image`] with a partition table.
#[derive(Debug, Clone)]
#[must_use]
pub struct Partition {
    pub(crate) name: String,
    pub(crate) start_lba: u64,
    pub(crate) end_lba: Option<u64>,
    pub(crate) fs_type: String,
    pub(crate) partition_type: u8,
    pub(crate) bootable: bool,
    pub(crate) entries: Vec<(Source, String)>,
}

impl Partition {
    /// A partition from sector `start_lba` to the end of the disk,
    /// formatted as `fs_type` (such as `fat32`), with MBR type `0x83`.
    pub fn new(name: impl Into<String>, start_lba: u64, fs_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            start_lba,
            end_lba: None,
            fs_type: fs_type.into(),
            partition_type: 0x83,
            bootable: false,
            entries: Vec::new(),
        }
    }

    /// End at sector `lba`, inclusive.
    pub fn end_lba(mut self, lba: u64) -> Self {
        self.end_lba = Some(lba);
        self
    }

    /// The MBR type byte: `0xEF` for an EFI system partition, `0x0C`
    /// for FAT32 with LBA.
    pub fn partition_type(mut self, byte: u8) -> Self {
        self.partition_type = byte;
        self
    }

    pub fn bootable(mut self) -> Self {
        self.bootable = true;
        self
    }

    /// Copy `source` to `dest` in the partition's filesystem.
    pub fn add(mut self, source: impl Into<Source>, dest: impl Into<String>) -> Self {
        self.entries.push((source.into(), dest.into()));
        self
    }
}

/// Run a host tool Quon builds to produce a file. The tool gets its
/// inputs as environment variables and `QUON_OUTPUT_PATH`, the path
/// it must write.
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// # let kernel = q.add(Package::new("kernel"));
/// let strip = q.add(Package::new("elf-strip").target(&Target::host()));
/// let stripped = q.add(
///     BuildStep::new("kernel-stripped", "kernel.elf")
///         .tool(&strip)
///         .input("ELF", &kernel),
/// );
/// # let _ = stripped;
/// ```
#[derive(Debug, Clone)]
#[must_use = "a build step does nothing until it is passed to `q.add`"]
pub struct BuildStep {
    pub(crate) name: String,
    pub(crate) output: String,
    pub(crate) tool: Option<Artifact>,
    pub(crate) inputs: BTreeMap<String, Source>,
    pub(crate) env: BTreeMap<String, String>,
}

impl BuildStep {
    /// A step named `name` that writes the file `output`, a plain file
    /// name inside the step's build directory.
    pub fn new(name: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            output: output.into(),
            tool: None,
            inputs: BTreeMap::new(),
            env: BTreeMap::new(),
        }
    }

    /// The binary that runs the step. Required.
    pub fn tool(mut self, tool: &Artifact) -> Self {
        self.tool = Some(tool.clone());
        self
    }

    /// Set `var` to the path of `source`: an artifact, or a file
    /// relative to the project root. The step reruns when it changes.
    pub fn input(mut self, var: impl Into<String>, source: impl Into<Source>) -> Self {
        self.inputs.insert(var.into(), source.into());
        self
    }

    /// Set `var` to `value` in the tool's environment. The step reruns
    /// when it changes.
    pub fn env(mut self, var: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(var.into(), value.into());
        self
    }
}

impl Declare for BuildStep {
    type Handle = Artifact;
    fn declare(self, q: &mut Quon) -> Artifact {
        q.add_build_step(self)
    }
}
