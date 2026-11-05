use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

/// One file owned by a package. `path` is relative to the package directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub kind: FileKind,
    pub mode: u32,
    pub sha256: Option<String>,
    pub link_target: Option<String>,
}

/// A Forge package another package was built against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepRef {
    pub name: String,
    /// The exact version the dependent was built against.
    pub version_id: String,
    /// Needed only while building (tools, code generators), not at runtime.
    #[serde(default)]
    pub build_only: bool,
}

fn yes() -> bool {
    true
}

/// Everything Forge knows about an installed package. Stored in the database
/// and mirrored to `<package dir>/.forge/manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// `<version>-<commit8>`; the store directory name.
    pub version_id: String,
    pub repo: String,
    /// The revision the user asked for (tag/branch/sha); `None` = default branch.
    pub rev_spec: Option<String>,
    pub commit: String,
    pub build_system: String,
    pub files: Vec<FileEntry>,
    pub executables: Vec<String>,
    /// Forge packages this one was built against (and needs at runtime unless `build_only`).
    #[serde(default)]
    pub deps: Vec<DepRef>,
    /// Host tools and pkg-config requirements checked before building.
    pub build_deps: Vec<String>,
    /// Installed because the user asked for it (true) or only as someone's dependency (false).
    #[serde(default = "yes")]
    pub explicit: bool,
    pub installed_at: i64,
    pub forge_version: String,
    /// How the commands are launched (isolation, environment, arguments).
    #[serde(default)]
    pub run: crate::recipe::RunSection,
    /// The effective recipe (TOML) so `update` rebuilds the same way.
    #[serde(default)]
    pub recipe: Option<String>,
}
