use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Every location Forge reads or writes lives under one root directory.
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Paths { root: root.into() }
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
        Ok(Paths { root: root.canonicalize()? })
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
