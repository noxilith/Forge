use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildSystem {
    Cargo,
    Cmake,
    Meson,
    Autotools,
    Go,
    Make,
    Custom,
}

impl BuildSystem {
    pub fn as_str(&self) -> &'static str {
        match self {
            BuildSystem::Cargo => "cargo",
            BuildSystem::Cmake => "cmake",
            BuildSystem::Meson => "meson",
            BuildSystem::Autotools => "autotools",
            BuildSystem::Go => "go",
            BuildSystem::Make => "make",
            BuildSystem::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "cargo" | "rust" => BuildSystem::Cargo,
            "cmake" => BuildSystem::Cmake,
            "meson" | "ninja" => BuildSystem::Meson,
            "autotools" | "autoconf" => BuildSystem::Autotools,
            "go" => BuildSystem::Go,
            "make" => BuildSystem::Make,
            "custom" => BuildSystem::Custom,
            other => bail!(
                "unknown build system '{other}' (cargo, cmake, meson, autotools, go, make, custom)"
            ),
        })
    }

    /// Guess from marker files. More specific systems win over plain `Makefile`
    /// because many projects ship a Makefile wrapper around cmake/meson/cargo.
    pub fn detect(src: &Path) -> Option<Self> {
        let has = |f: &str| src.join(f).exists();
        if has("Cargo.toml") {
            Some(BuildSystem::Cargo)
        } else if has("meson.build") {
            Some(BuildSystem::Meson)
        } else if has("CMakeLists.txt") {
            Some(BuildSystem::Cmake)
        } else if has("configure") || has("configure.ac") || has("configure.in") || has("autogen.sh")
        {
            Some(BuildSystem::Autotools)
        } else if has("go.mod") {
            Some(BuildSystem::Go)
        } else if has("Makefile") || has("makefile") || has("GNUmakefile") {
            Some(BuildSystem::Make)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_priority() {
        let d = std::env::temp_dir().join(format!("forge-detect-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(BuildSystem::detect(&d), None);
        std::fs::write(d.join("Makefile"), "").unwrap();
        assert_eq!(BuildSystem::detect(&d), Some(BuildSystem::Make));
        std::fs::write(d.join("CMakeLists.txt"), "").unwrap();
        assert_eq!(BuildSystem::detect(&d), Some(BuildSystem::Cmake));
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        assert_eq!(BuildSystem::detect(&d), Some(BuildSystem::Cargo));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
