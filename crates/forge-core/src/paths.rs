use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Every location Forge reads or writes lives under one root directory.
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
    system: bool,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Paths { root: root.into(), system: false }
    }

    /// `$FORGE_ROOT`, else `$XDG_DATA_HOME/forge`, else `~/.local/share/forge`.
    /// The root is created and canonicalised so every path string we hand to
    /// builds and symlinks is stable.
    pub fn from_env() -> Result<Self> {
        let root = if let Some(r) = std::env::var_os("FORGE_ROOT").filter(|v| !v.is_empty()) {
            PathBuf::from(r)
        } else {
            let base = match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
                Some(p) if p.is_absolute() => p,
                _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                    .join(".local/share"),
            };
            base.join("forge")
        };
        if !root.is_absolute() {
            bail!("FORGE_ROOT must be an absolute path (got {})", root.display());
        }
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Paths { root: root.canonicalize()?, system: false })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Immutable per-package directories: `store/<name>/<version-id>`.
    pub fn store(&self) -> PathBuf {
        self.root.join("store")
    }
    /// `current/<name>` -> active store directory.
    pub fn current(&self) -> PathBuf {
        self.root.join("current")
    }
    /// The only directory users put on PATH.
    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }
    pub fn sources(&self) -> PathBuf {
        self.root.join("sources")
    }
    pub fn builds(&self) -> PathBuf {
        self.root.join("builds")
    }
    pub fn trash(&self) -> PathBuf {
        self.root.join("trash")
    }
    pub fn recipes(&self) -> PathBuf {
        self.root.join("recipes")
    }
    pub fn db_dir(&self) -> PathBuf {
        self.root.join("db")
    }
    pub fn db_file(&self) -> PathBuf {
        self.db_dir().join("forge.sqlite")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("lock")
    }

    pub fn package_dir(&self, name: &str, version_id: &str) -> PathBuf {
        self.store().join(name).join(version_id)
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for d in [
            self.store(),
            self.current(),
            self.bin(),
            self.sources(),
            self.builds(),
            self.trash(),
            self.recipes(),
            self.db_dir(),
        ] {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }
}

/// Where a system-wide install lives unless `FORGE_ROOT` says otherwise. Compiled programs have
/// their install path built in, so a distribution that ships prebuilt packages fixes this.
pub const SYSTEM_ROOT: &str = "/opt/forge";

impl Paths {
    /// The shared install that root manages (`forge --system`): `$FORGE_ROOT`, else [`SYSTEM_ROOT`].
    /// Created only when `create` is set (root, changing things); otherwise it must exist.
    pub fn system_from_env(create: bool) -> Result<Self> {
        let root = std::env::var_os("FORGE_ROOT").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(SYSTEM_ROOT));
        if !root.is_absolute() {
            bail!("FORGE_ROOT must be an absolute path (got {})", root.display());
        }
        if create {
            std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
            // Everyone must be able to run what is installed.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))?;
        }
        let root = root.canonicalize().with_context(|| format!("no system-wide Forge install at {} yet (the administrator creates it with `sudo forge --system install ...`)", root.display()))?;
        Ok(Paths { root, system: true })
    }

    /// Is this the shared, system-wide install rather than one user's?
    pub fn is_system(&self) -> bool {
        self.system
    }

    /// A package's data directory for the current user. In a user install it is part of the
    /// package's own tree; in the system install it is in the user's home, resolved when the
    /// program starts (`FORGE_DATA` in the launcher script), because one install serves everyone.
    pub fn user_data_dir(&self, name: &str) -> PathBuf {
        if self.system {
            let base = match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
                Some(p) if p.is_absolute() => p,
                _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"),
            };
            base.join("forge/data").join(name)
        } else {
            self.root.join("data").join(name)
        }
    }
}

impl Paths {
    /// A system-wide install rooted at `root` (for tests and tools; normally `system_from_env`).
    pub fn new_system(root: impl Into<PathBuf>) -> Self {
        Paths { root: root.into(), system: true }
    }
}
