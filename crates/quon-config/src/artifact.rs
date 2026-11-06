//! Handles to things Quon builds or fetches, and the sources that
//! ESPs, images and build steps copy from.

/// A handle to something that produces a file: a crate, a build step,
/// a download, an ESP or a disk image. [`Quon::add`](crate::Quon::add)
/// returns one, and other declarations take it by reference, so a
/// reference to something that was never declared does not compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub(crate) kind: ArtifactKind,
    pub(crate) name: String,
    /// The declaration this artifact belongs to: the crate or package
    /// for a crate artifact (see [`Artifact::bin`]), else `name`.
    pub(crate) owner: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactKind {
    Crate,
    BuildStep,
    Download,
    Esp,
    Image,
    Dependency,
}

impl ArtifactKind {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Crate => "crate",
            Self::BuildStep => "build step",
            Self::Download => "download",
            Self::Esp => "ESP",
            Self::Image => "image",
            Self::Dependency => "dependency",
        }
    }
}

impl Artifact {
    pub(crate) fn new(kind: ArtifactKind, name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            kind,
            owner: name.clone(),
            name,
        }
    }

    /// The name Quon gives this artifact in its output and messages.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Another binary of the same workspace package, for a package
    /// with more than one `[[bin]]`. The handle `q.add` returned for a
    /// package names the bin (or lib) that has the package's name.
    ///
    /// ```
    /// # use quon_config::prelude::*;
    /// # let mut q = Quon::new();
    /// let tools = q.add(Package::new("tools"));
    /// let mkimage = tools.bin("mkimage");
    /// q.run(&mkimage);
    /// ```
    #[must_use]
    pub fn bin(&self, name: impl Into<String>) -> Artifact {
        Artifact {
            kind: self.kind,
            name: name.into(),
            owner: self.owner.clone(),
        }
    }

    pub(crate) fn is_buildable(&self) -> bool {
        matches!(self.kind, ArtifactKind::Crate | ArtifactKind::BuildStep)
    }
}

/// What an ESP, image or build step copies from: an [`Artifact`], a
/// file in the project, or literal bytes.
///
/// `&Artifact` converts into a `Source`, and so does a string, which
/// names a file relative to the project root:
///
/// ```
/// # use quon_config::prelude::*;
/// # let mut q = Quon::new();
/// # let kernel = q.add(Package::new("kernel"));
/// Esp::new("boot")
///     .add(&kernel, "boot/kernel")
///     .add("limine.conf", "limine.conf");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Artifact(Artifact),
    /// A file relative to the project root.
    File(String),
    /// Literal bytes, for [`Image::raw_write`](crate::Image::raw_write).
    Bytes(Vec<u8>),
}

impl From<&Artifact> for Source {
    fn from(a: &Artifact) -> Self {
        Self::Artifact(a.clone())
    }
}

impl From<Artifact> for Source {
    fn from(a: Artifact) -> Self {
        Self::Artifact(a)
    }
}

impl From<&str> for Source {
    fn from(path: &str) -> Self {
        Self::File(path.into())
    }
}

impl From<String> for Source {
    fn from(path: String) -> Self {
        Self::File(path)
    }
}

impl From<Vec<u8>> for Source {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes)
    }
}
