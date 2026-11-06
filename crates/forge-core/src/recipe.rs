use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// A build recipe. Everything is optional: an empty recipe means
/// "detect the build system and use its defaults".
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    #[serde(default)]
    pub package: PackageSection,
    #[serde(default)]
    pub build: BuildSection,
    #[serde(default)]
    pub install: InstallSection,
    #[serde(default)]
    pub run: RunSection,
    /// Directory the recipe was read from (where `patches` live). Not part of the file.
    #[serde(skip)]
    pub base_dir: Option<std::path::PathBuf>,
}

/// How the installed commands are launched: where the program keeps its
/// configuration and data, and what it is started with. Forge generates a
/// small launcher script for each exported command from this.
///
/// `{data}` in values stands for `$FORGE_ROOT/data/<package>`, a directory
/// owned by the package and deleted with it. `{dep:NAME}` stands for the
/// directory of Forge package NAME (for example a QML or plugin path).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunSection {
    /// `xdg` (default): point XDG_CONFIG_HOME / XDG_DATA_HOME / XDG_CACHE_HOME /
    /// XDG_STATE_HOME into `{data}`. `home`: additionally set HOME. `none`: leave
    /// the environment alone. Anything the program starts inherits the redirect, so
    /// use `none` plus `env`/`args` for programs that launch other programs
    /// (compositors, terminals, shells).
    pub isolate: Option<String>,
    /// Extra environment variables for the program.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Arguments placed before the user's own.
    #[serde(default)]
    pub args: Vec<String>,
    /// Restrict `args` to these exported commands (default: all of them).
    #[serde(default)]
    pub args_commands: Vec<String>,
    /// Extra directories to create under `{data}` before starting.
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Default files copied into `{data}` if missing: `{ "config/x/y.conf" = "share/x/default.conf" }`
    /// (destination relative to `{data}`, source relative to the package).
    #[serde(default)]
    pub seed: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    None,
    Xdg,
    Home,
}

impl RunSection {
    pub fn isolation(&self) -> Result<Isolation> {
        Ok(match self.isolate.as_deref() {
            None | Some("xdg") => Isolation::Xdg,
            Some("home") => Isolation::Home,
            Some("none") => Isolation::None,
            Some(o) => bail!("run.isolate must be \"xdg\", \"home\" or \"none\" (got \"{o}\")"),
        })
    }

    /// Does any exported command need a launcher script?
    pub fn needs_launcher(&self) -> bool {
        self.isolation().map(|i| i != Isolation::None).unwrap_or(true)
            || !self.env.is_empty()
            || !self.args.is_empty()
            || !self.dirs.is_empty()
            || !self.seed.is_empty()
    }

    fn validate(&self) -> Result<()> {
        self.isolation()?;
        for k in self.env.keys() {
            if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || k.starts_with("FORGE_") {
                bail!("invalid run.env name '{k}'");
            }
        }
        let rel = |what: &str, p: &str| -> Result<()> {
            let path = Path::new(p);
            if p.is_empty() || path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                bail!("{what} '{p}' must be a relative path without '..'");
            }
            Ok(())
        };
        for d in &self.dirs {
            rel("run.dirs entry", d)?;
        }
        for (dest, src) in &self.seed {
            rel("run.seed destination", dest)?;
            rel("run.seed source", src)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSection {
    pub name: Option<String>,
    pub repo: Option<String>,
    /// Tag, branch or commit to build. Default: the repository's default branch.
    pub rev: Option<String>,
    /// If set, the resolved commit must start with this value.
    pub commit: Option<String>,
    /// Forge packages this one needs, at build time and at runtime. Missing
    /// ones are built and installed first (from recipes), and removed again
    /// when nothing needs them.
    #[serde(default, alias = "runtime_deps")]
    pub deps: Vec<String>,

    /// A release tarball to build instead of a git repository (give `archive`
    /// *or* `repo`). `https` only, and the SHA-256 is mandatory: the download
    /// must match it exactly.
    pub archive: Option<String>,
    pub sha256: Option<String>,
    /// The package's version. Required with `archive` (a git source gets its
    /// version from its tags).
    pub version: Option<String>,
    /// Leading directories to drop when unpacking an archive (default 1, as
    /// most tarballs wrap everything in one directory).
    pub strip_components: Option<u32>,
    /// Patch files, relative to the recipe's directory, applied in order
    /// (inside the sandbox) before building.
    #[serde(default)]
    pub patches: Vec<String>,
    /// `-p` level for the patches (default 1).
    pub patch_strip: Option<u32>,
    /// Shown by `forge search`.
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSection {
    /// cargo | cmake | meson | autotools | go | make | custom
    pub system: Option<String>,
    /// Shell commands for `custom` builds (run offline, inside the sandbox,
    /// with `$FORGE_PREFIX`, `$FORGE_STAGE`, `$FORGE_SRC`, `$FORGE_JOBS` set).
    #[serde(default)]
    pub steps: Vec<String>,
    /// Extra commands run first with network access (dependency downloads).
    #[serde(default)]
    pub fetch: Vec<String>,
    /// Extra arguments for the backend's configure/build command.
    #[serde(default)]
    pub configure_args: Vec<String>,
    /// Like `configure_args`, but each entry is a shell fragment that is NOT quoted, so it may use
    /// variables and `$(...)`: for options only a test on the build machine can decide.
    #[serde(default)]
    pub configure_sh: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Forge packages needed only while building (code generators and the
    /// like). They are installed like `package.deps` but not linked at runtime.
    #[serde(default)]
    pub deps: Vec<String>,
    /// Host tools that must exist before building.
    #[serde(default)]
    pub tools: Vec<String>,
    /// pkg-config requirements, e.g. `"wayland-server >= 1.22"`.
    #[serde(default)]
    pub pkgconfig: Vec<String>,
    /// Build from this subdirectory of the repository.
    pub subdir: Option<String>,
    /// Go: the packages to install (default `./...`, every program in the module).
    #[serde(default)]
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSection {
    /// Files (relative to the package dir) to link into `$FORGE_ROOT/bin`.
    /// Entries may be exact paths or `dir/*`. Default: everything in `bin/`.
    pub exports: Option<Vec<String>>,
    /// Extra commands run after the backend's install step, with `DESTDIR` set.
    #[serde(default)]
    pub post: Vec<String>,
}

impl Recipe {
    pub fn parse(text: &str) -> Result<Self> {
        let r: Recipe = toml::from_str(text).context("invalid recipe")?;
        r.validate()?;
        Ok(r)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading recipe {}", path.display()))?;
        let mut r = Self::parse(&text).with_context(|| format!("in {}", path.display()))?;
        r.base_dir = path.parent().map(Path::to_path_buf);
        Ok(r)
    }

    /// Where the source comes from: the archive URL, else the repository.
    pub fn source_url(&self) -> Option<&str> {
        self.package.archive.as_deref().or(self.package.repo.as_deref())
    }

    /// The patch files, resolved against the recipe's directory and checked to exist.
    pub fn patch_files(&self) -> Result<Vec<std::path::PathBuf>> {
        if self.package.patches.is_empty() {
            return Ok(Vec::new());
        }
        let base = self.base_dir.as_deref().context("this recipe lists patches but was not read from a file")?;
        self.package
            .patches
            .iter()
            .map(|p| {
                let f = base.join(p);
                if !f.is_file() {
                    bail!("patch {} not found next to the recipe", f.display());
                }
                Ok(f)
            })
            .collect()
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    fn validate_source(&self) -> Result<()> {
        let p = &self.package;
        if p.archive.is_some() && p.repo.is_some() {
            bail!("package.archive and package.repo are alternatives: give one");
        }
        if let Some(url) = &p.archive {
            if !(url.starts_with("https://") || url.starts_with("file://")) {
                bail!("package.archive must be an https:// URL (got {url})");
            }
            match &p.sha256 {
                Some(h) if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) => {}
                Some(_) => bail!("package.sha256 must be 64 hexadecimal characters"),
                None => bail!("package.archive needs package.sha256: a download is only accepted if it matches"),
            }
            match &p.version {
                Some(v) => crate::util::validate_version_id(v)?,
                None => bail!("package.archive needs package.version"),
            }
        } else if p.sha256.is_some() || p.strip_components.is_some() {
            bail!("package.sha256 and package.strip_components only apply to package.archive");
        }
        if let Some(n) = p.strip_components {
            if n > 8 {
                bail!("package.strip_components {n} is unreasonable");
            }
        }
        if let Some(n) = p.patch_strip {
            if n > 8 {
                bail!("package.patch_strip {n} is unreasonable");
            }
        }
        for f in &p.patches {
            let path = Path::new(f);
            if f.is_empty() || path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                bail!("package.patches entry '{f}' must be a relative path without '..'");
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if let Some(n) = &self.package.name {
            crate::util::validate_name(n)?;
        }
        self.run.validate()?;
        self.validate_source()?;
        for d in self.package.deps.iter().chain(&self.build.deps) {
            crate::util::validate_name(d)?;
        }
        if let Some(s) = &self.build.system {
            crate::BuildSystem::parse(s)?;
        }
        if let Some(d) = &self.build.subdir {
            let p = Path::new(d);
            if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!("build.subdir must be a relative path without '..'");
            }
        }
        for k in self.build.env.keys() {
            if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("invalid environment variable name '{k}'");
            }
            if k.starts_with("FORGE_") {
                bail!("recipes may not set FORGE_* variables ('{k}')");
            }
        }
        for e in self.install.exports.iter().flatten() {
            let p = Path::new(e);
            if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!("install.exports entry '{e}' must be relative without '..'");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let r = Recipe::parse(
            r#"
            [package]
            name = "x"
            repo = "https://example.com/x"
            [build]
            system = "cmake"
            configure_args = ["-DA=1"]
            [install]
            exports = ["bin/x"]
            "#,
        )
        .unwrap();
        let again = Recipe::parse(&r.to_toml().unwrap()).unwrap();
        assert_eq!(again.build.system.as_deref(), Some("cmake"));
        assert_eq!(again.install.exports.unwrap(), vec!["bin/x"]);
    }

    #[test]
    fn rejects_bad() {
        assert!(Recipe::parse("[build]\nunknown = 1").is_err());
        assert!(Recipe::parse("[build]\nsubdir = \"../x\"").is_err());
        assert!(Recipe::parse("[build.env]\nFORGE_PREFIX = \"/\"").is_err());
        assert!(Recipe::parse("[package]\nname = \"../x\"").is_err());
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;

    #[test]
    fn run_section() {
        let r = Recipe::parse(
            r#"
            [run]
            isolate = "none"
            args = ["--config", "{data}/c.conf"]
            dirs = ["config/x"]
            [run.env]
            X_CONFIG = "{data}/config/x"
            [run.seed]
            "config/x/a.conf" = "share/x/a.conf"
            "#,
        )
        .unwrap();
        assert_eq!(r.run.isolation().unwrap(), Isolation::None);
        assert!(r.run.needs_launcher());
        // Defaults: isolation on, so a launcher is needed.
        assert_eq!(Recipe::default().run.isolation().unwrap(), Isolation::Xdg);
        assert!(Recipe::default().run.needs_launcher());
        assert!(!Recipe::parse("[run]\nisolate = \"none\"").unwrap().run.needs_launcher());
        for bad in [
            "[run]\nisolate = \"yes\"",
            "[run]\ndirs = [\"../x\"]",
            "[run]\ndirs = [\"/abs\"]",
            "[run.seed]\n\"../x\" = \"y\"",
            "[run.seed]\n\"x\" = \"/etc/passwd\"",
            "[run.env]\nFORGE_X = \"1\"",
            "[run.env]\n\"A B\" = \"1\"",
        ] {
            assert!(Recipe::parse(bad).is_err(), "{bad}");
        }
        let again = Recipe::parse(&r.to_toml().unwrap()).unwrap();
        assert_eq!(again.run, r.run);
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn archive_sources() {
        let r = Recipe::parse(&format!(
            "[package]\nname = \"fmt\"\nversion = \"12.1.0\"\narchive = \"https://github.com/fmtlib/fmt/archive/12.1.0.tar.gz\"\nsha256 = \"{H}\"\npatches = [\"patches/a.patch\"]\ndescription = \"x\"\n[build]\nsystem = \"cmake\"\n"
        ))
        .unwrap();
        assert_eq!(r.source_url(), Some("https://github.com/fmtlib/fmt/archive/12.1.0.tar.gz"));
        assert_eq!(r.package.patches, vec!["patches/a.patch"]);
        // A git recipe's source is its repo.
        assert_eq!(Recipe::parse("[package]\nrepo = \"https://x/y\"").unwrap().source_url(), Some("https://x/y"));
    }

    #[test]
    fn archive_sources_are_checked() {
        let base = format!("name = \"x\"\nversion = \"1.0\"\narchive = \"https://x/y.tar.gz\"\nsha256 = \"{H}\"\n");
        assert!(Recipe::parse(&format!("[package]\n{base}")).is_ok());
        for (bad, why) in [
            (format!("[package]\n{base}repo = \"https://x\"\n"), "both archive and repo"),
            (format!("[package]\nname = \"x\"\nversion = \"1.0\"\narchive = \"https://x/y.tar.gz\"\n"), "no checksum"),
            (format!("[package]\nname = \"x\"\nversion = \"1.0\"\narchive = \"https://x/y.tar.gz\"\nsha256 = \"abc\"\n"), "short checksum"),
            (format!("[package]\nname = \"x\"\narchive = \"https://x/y.tar.gz\"\nsha256 = \"{H}\"\n"), "no version"),
            (format!("[package]\nname = \"x\"\nversion = \"1.0\"\narchive = \"http://x/y.tar.gz\"\nsha256 = \"{H}\"\n"), "plain http"),
            (format!("[package]\nname = \"x\"\nversion = \"1.0\"\narchive = \"ftp://x/y\"\nsha256 = \"{H}\"\n"), "ftp"),
            (format!("[package]\n{base}patches = [\"../evil.patch\"]\n"), "patch escapes"),
            (format!("[package]\n{base}patches = [\"/etc/passwd\"]\n"), "absolute patch"),
            ("[package]\nrepo = \"https://x\"\nsha256 = \"abc\"\n".to_string(), "sha256 without archive"),
        ] {
            assert!(Recipe::parse(&bad).is_err(), "{why}");
        }
    }

    #[test]
    fn patch_files_are_resolved_next_to_the_recipe() {
        let d = std::env::temp_dir().join(format!("forge-patchrecipe-{}", std::process::id()));
        std::fs::create_dir_all(d.join("patches")).unwrap();
        std::fs::write(d.join("patches/a.patch"), "x").unwrap();
        std::fs::write(
            d.join("recipe.toml"),
            format!("[package]\nname = \"x\"\nversion = \"1\"\narchive = \"https://x/y.tar.gz\"\nsha256 = \"{H}\"\npatches = [\"patches/a.patch\"]\n"),
        )
        .unwrap();
        let r = Recipe::from_file(&d.join("recipe.toml")).unwrap();
        assert_eq!(r.patch_files().unwrap(), vec![d.join("patches/a.patch")]);
        std::fs::remove_file(d.join("patches/a.patch")).unwrap();
        assert!(r.patch_files().unwrap_err().to_string().contains("not found"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
