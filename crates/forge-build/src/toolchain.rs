//! Toolchains Forge provides itself, so a build never depends on what the
//! machine happens to have installed: Go and Rust.
//!
//! If the machine's compiler is missing, broken or too old for a project,
//! Forge downloads the official release (go.dev, static.rust-lang.org), checks
//! it against the checksum the publisher lists, unpacks it under
//! `$FORGE_ROOT/toolchains/<tool>-<version>/`, and mounts it read-only into
//! the sandboxed build. It is a cache owned by Forge: not part of any package,
//! reused by every later build, removable with `forge toolchain remove`.
//!
//! `FORGE_TOOLCHAINS` picks the policy:
//! * `auto` (default): use the machine's compiler if it works and is new enough
//! * `managed`: always use Forge's (hermetic: the same compiler on every machine)
//! * `host`: never download; fail if the machine's compiler doesn't do
//!
//! Compilers cannot be compiled into the `forge` binary. A Go compiler is built
//! by an older Go compiler, so the chain has to start from a published binary.

use anyhow::{Context, Result, bail};
use forge_core::util::rm_rf;
use forge_core::{BuildSystem, Paths, ui};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Go,
    Rust,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Go => "go",
            Tool::Rust => "rust",
        }
    }
    pub fn parse(s: &str) -> Result<Tool> {
        match s {
            "go" => Ok(Tool::Go),
            "rust" | "cargo" | "rustc" => Ok(Tool::Rust),
            o => bail!("no toolchain called '{o}' (available: go, rust)"),
        }
    }
    /// Executables whose absence this toolchain makes irrelevant.
    pub fn provides(self) -> &'static [&'static str] {
        match self {
            Tool::Go => &["go"],
            Tool::Rust => &["cargo", "rustc"],
        }
    }
    pub fn source(self) -> &'static str {
        match self {
            Tool::Go => "go.dev",
            Tool::Rust => "static.rust-lang.org",
        }
    }
}

// ---- versions ----

#[derive(Clone, Debug, Eq)]
pub struct Version(pub Vec<u64>);

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Version {
    /// `1.27.3`, `go1.27.3`, `v1.2`, `1.99.1 (58cf8f9c3 2026-10-13)`, `1.27rc1` -> the leading numbers.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches("go").trim_start_matches('v');
        let head = s.split(|c: char| !(c.is_ascii_digit() || c == '.')).next()?;
        let parts: Option<Vec<u64>> = head.trim_end_matches('.').split('.').map(|p| p.parse().ok()).collect();
        parts.filter(|p| !p.is_empty()).map(Version)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // 1.27 and 1.27.0 are the same version.
        let n = self.0.len().max(other.0.len());
        for i in 0..n {
            let (a, b) = (self.0.get(i).copied().unwrap_or(0), other.0.get(i).copied().unwrap_or(0));
            if a != b {
                return a.cmp(&b);
            }
        }
        Ordering::Equal
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.iter().map(u64::to_string).collect::<Vec<_>>().join("."))
    }
}

// ---- policy ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Auto,
    Host,
    Managed,
}

pub fn mode() -> Mode {
    match std::env::var("FORGE_TOOLCHAINS").as_deref() {
        Ok("host") => Mode::Host,
        Ok("managed") => Mode::Managed,
        _ => Mode::Auto,
    }
}

// ---- what a project needs ----

#[derive(Clone, Debug)]
pub struct Requirement {
    pub tool: Tool,
    /// Oldest acceptable version (`None`: any).
    pub min: Option<Version>,
    /// Where the requirement comes from, for messages.
    pub why: String,
}

/// What the project in `src` needs, from its own files.
pub fn requirement(system: BuildSystem, src: &Path) -> Result<Option<Requirement>> {
    match system {
        BuildSystem::Go => Ok(go_requirement(&fs::read_to_string(src.join("go.mod")).unwrap_or_default())),
        BuildSystem::Cargo => rust_requirement(src),
        _ => Ok(None),
    }
}

pub fn go_requirement(go_mod: &str) -> Option<Requirement> {
    let (mut min, mut why) = (None::<Version>, String::new());
    for line in go_mod.lines() {
        let l = line.split("//").next().unwrap_or("").trim();
        let (key, val) = match l.split_once(char::is_whitespace) {
            Some(kv) => kv,
            None => continue,
        };
        if key == "go" || key == "toolchain" {
            if let Some(v) = Version::parse(val) {
                if min.as_ref().is_none_or(|m| v > *m) {
                    min = Some(v);
                    why = format!("go.mod says `{l}`");
                }
            }
        }
    }
    Some(Requirement { tool: Tool::Go, min, why: if why.is_empty() { "a Go project".into() } else { why } })
}

fn rust_requirement(src: &Path) -> Result<Option<Requirement>> {
    let mut min: Option<Version> = None;
    let mut why = "a Rust project".to_string();
    let mut raise = |v: Version, w: String| {
        if min.as_ref().is_none_or(|m| v > *m) {
            min = Some(v);
            why = w;
        }
    };

    if let Ok(text) = fs::read_to_string(src.join("Cargo.toml")) {
        if let Ok(doc) = text.parse::<toml::Table>() {
            let pkg = doc.get("package").and_then(|p| p.as_table());
            let ws = doc.get("workspace").and_then(|w| w.get("package")).and_then(|p| p.as_table());
            for t in [pkg, ws].into_iter().flatten() {
                if let Some(v) = t.get("rust-version").and_then(|v| v.as_str()).and_then(Version::parse) {
                    raise(v.clone(), format!("Cargo.toml says rust-version = \"{v}\""));
                }
                if t.get("edition").and_then(|e| e.as_str()) == Some("2024") {
                    raise(Version(vec![1, 85]), "Cargo.toml uses edition 2024".into());
                }
            }
        }
    }
    // rust-toolchain.toml / rust-toolchain: a version is a minimum; stable is anything; nightly we can't provide.
    for f in ["rust-toolchain.toml", "rust-toolchain"] {
        let Ok(text) = fs::read_to_string(src.join(f)) else { continue };
        let channel = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|t| t.get("toolchain").and_then(|t| t.get("channel")).and_then(|c| c.as_str()).map(str::to_string))
            .unwrap_or_else(|| text.trim().to_string());
        if channel.starts_with("nightly") || channel.starts_with("beta") {
            bail!("{f} asks for the {channel} channel, which Forge can't provide (only stable releases)");
        }
        if let Some(v) = Version::parse(&channel) {
            raise(v, format!("{f} says {channel}"));
        }
    }
    Ok(Some(Requirement { tool: Tool::Rust, min, why }))
}

fn find_in(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| fs::metadata(p).map(|m| m.is_file()).unwrap_or(false))
}

/// The version of the machine's compiler, if it exists and actually runs.
pub fn host_version(tool: Tool, dirs: &[PathBuf]) -> Option<Version> {
    let (exe, arg, idx) = match tool {
        Tool::Go => ("go", "version", 2), // go version go1.27.3 linux/amd64
        Tool::Rust => ("rustc", "--version", 1), // rustc 1.99.1 (hash date)
    };
    let path = find_in(dirs, exe)?;
    let out = Command::new(path).arg(arg).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let v = Version::parse(text.split_whitespace().nth(idx)?)?;
    if tool == Tool::Rust && find_in(dirs, "cargo").is_none() {
        return None;
    }
    Some(v)
}

/// Is the machine's compiler good enough? Returns the reason when it isn't.
pub fn host_problem(req: &Requirement, dirs: &[PathBuf]) -> Option<String> {
    match host_version(req.tool, dirs) {
        None => Some(format!("this machine's {} is missing or doesn't run", req.tool.name())),
        Some(h) => match &req.min {
            Some(m) if h < *m => Some(format!("this machine has {} {h}, but {m} is needed ({})", req.tool.name(), req.why)),
            _ => None,
        },
    }
}

// ---- downloads ----

#[derive(Clone, Debug)]
pub struct Download {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub tool: Tool,
    pub version: Version,
    pub dir: PathBuf,
    pub cached: bool,
    pub downloads: Vec<Download>,
}

impl Plan {
    pub fn label(&self) -> String {
        format!("{} {}", self.tool.name(), self.version)
    }
    pub fn download_bytes(&self) -> u64 {
        self.downloads.iter().filter_map(|d| d.size).sum()
    }
}

fn go_arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        a => bail!("Forge can download toolchains for x86_64 and aarch64 only (this is {a})"),
    }
}

fn rust_target() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        a => bail!("Forge can download toolchains for x86_64 and aarch64 only (this is {a})"),
    }
}

#[derive(Deserialize)]
struct GoRelease {
    version: String,
    stable: bool,
    files: Vec<GoFile>,
}
#[derive(Deserialize)]
struct GoFile {
    filename: String,
    os: String,
    arch: String,
    kind: String,
    sha256: String,
    size: u64,
}

/// Newest stable Go release at least `min` that has a Linux archive for `arch`.
pub fn go_pick(index_json: &str, min: Option<&Version>, arch: &str, dl_base: &str) -> Result<(Version, Download)> {
    let releases: Vec<GoRelease> = serde_json::from_str(index_json).context("reading the Go release index")?;
    let mut best: Option<(Version, Download)> = None;
    for r in releases.iter().filter(|r| r.stable) {
        let Some(v) = Version::parse(&r.version) else { continue };
        if min.is_some_and(|m| v < *m) || best.as_ref().is_some_and(|(b, _)| v <= *b) {
            continue;
        }
        let Some(f) = r.files.iter().find(|f| f.os == "linux" && f.arch == arch && f.kind == "archive" && f.filename.ends_with(".tar.gz"))
        else {
            continue;
        };
        if f.sha256.len() != 64 {
            continue;
        }
        best = Some((v, Download { name: f.filename.clone(), url: format!("{dl_base}{}", f.filename), sha256: f.sha256.clone(), size: Some(f.size) }));
    }
    best.with_context(|| match min {
        Some(m) => format!("no stable Go release >= {m} found for linux/{arch}"),
        None => format!("no stable Go release found for linux/{arch}"),
    })
}

/// The stable Rust release: its version and the rustc, cargo and rust-std downloads.
pub fn rust_pick(manifest: &str, target: &str, min: Option<&Version>) -> Result<(Version, Vec<Download>)> {
    let doc: toml::Table = manifest.parse().context("reading the Rust release manifest")?;
    let pkg = doc.get("pkg").and_then(|p| p.as_table()).context("manifest has no [pkg]")?;
    let version = pkg
        .get("rust")
        .and_then(|r| r.get("version"))
        .and_then(|v| v.as_str())
        .and_then(Version::parse)
        .context("manifest has no rust version")?;
    if min.is_some_and(|m| version < *m) {
        bail!("the newest stable Rust is {version}, older than the {} needed", min.unwrap());
    }
    let mut out = Vec::new();
    for comp in ["rustc", "cargo", "rust-std"] {
        let e = pkg
            .get(comp)
            .and_then(|c| c.get("target"))
            .and_then(|t| t.get(target))
            .and_then(|t| t.as_table())
            .with_context(|| format!("manifest has no {comp} for {target}"))?;
        if e.get("available").and_then(|a| a.as_bool()) != Some(true) {
            bail!("{comp} is not available for {target} in this release");
        }
        let url = e.get("xz_url").and_then(|u| u.as_str()).with_context(|| format!("{comp}: no xz_url"))?;
        let hash = e.get("xz_hash").and_then(|h| h.as_str()).with_context(|| format!("{comp}: no xz_hash"))?;
        if hash.len() != 64 {
            bail!("{comp}: unexpected hash format");
        }
        let name = url.rsplit('/').next().unwrap_or(comp).to_string();
        out.push(Download { name, url: url.to_string(), sha256: hash.to_string(), size: None });
    }
    Ok((version, out))
}

fn toolchains_dir(paths: &Paths) -> PathBuf {
    paths.root().join("toolchains")
}

fn entry_name(tool: Tool, v: &Version) -> String {
    format!("{}-{v}", tool.name())
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub tool: Tool,
    pub version: Version,
    pub name: String,
    pub dir: PathBuf,
}

pub fn installed(paths: &Paths) -> Vec<Installed> {
    let mut v = Vec::new();
    for e in fs::read_dir(toolchains_dir(paths)).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((t, ver)) = name.split_once('-') else { continue };
        let (Ok(tool), Some(version)) = (Tool::parse(t), Version::parse(ver)) else { continue };
        if e.path().join(".forge-toolchain.json").is_file() {
            v.push(Installed { tool, version, name, dir: e.path() });
        }
    }
    v.sort_by(|a, b| (a.tool.name(), &a.version).cmp(&(b.tool.name(), &b.version)));
    v
}

pub fn dir_size(p: &Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(p) else { return 0 };
    if m.is_dir() {
        fs::read_dir(p).into_iter().flatten().flatten().map(|e| dir_size(&e.path())).sum()
    } else {
        m.len()
    }
}

/// Decide what to use for `req`: a cached toolchain if one is new enough
/// (no network), else the newest release from the publisher.
pub fn resolve(paths: &Paths, req: &Requirement) -> Result<Plan> {
    let cached = installed(paths)
        .into_iter()
        .filter(|i| i.tool == req.tool && req.min.as_ref().is_none_or(|m| i.version >= *m))
        .max_by(|a, b| a.version.cmp(&b.version));
    if let Some(c) = cached {
        return Ok(Plan { tool: c.tool, version: c.version, dir: c.dir, cached: true, downloads: vec![] });
    }
    let (version, downloads) = match req.tool {
        Tool::Go => {
            let index = std::env::var("FORGE_GO_INDEX").ok();
            let base = std::env::var("FORGE_GO_DL").unwrap_or_else(|_| "https://go.dev/dl/".into());
            let allow_file = index.is_some();
            let url = index.unwrap_or_else(|| "https://go.dev/dl/?mode=json".into());
            let json = fetch_text(&url, allow_file).context("fetching the Go release index")?;
            let (v, d) = go_pick(&json, req.min.as_ref(), go_arch()?, &base)?;
            (v, vec![d])
        }
        Tool::Rust => {
            let base = std::env::var("FORGE_RUST_DIST").unwrap_or_else(|_| "https://static.rust-lang.org/dist".into());
            let allow_file = std::env::var("FORGE_RUST_DIST").is_ok();
            let text = fetch_text(&format!("{base}/channel-rust-stable.toml"), allow_file)
                .context("fetching the Rust release manifest")?;
            rust_pick(&text, rust_target()?, req.min.as_ref())?
        }
    };
    let dir = toolchains_dir(paths).join(entry_name(req.tool, &version));
    Ok(Plan { tool: req.tool, version, dir, cached: false, downloads })
}

fn curl_args(url: &str, allow_file: bool) -> Result<Vec<String>> {
    let proto = if url.starts_with("file://") {
        if !allow_file {
            bail!("refusing a file:// download URL");
        }
        "=file"
    } else if url.starts_with("https://") {
        "=https"
    } else {
        bail!("refusing to download over a non-HTTPS URL: {url}");
    };
    Ok(["--fail", "--silent", "--show-error", "--location", "--proto", proto, "--proto-redir", "=https,file", "--connect-timeout", "20", "--max-time", "3600"]
        .iter()
        .map(|s| s.to_string())
        .collect())
}

fn fetch_text(url: &str, allow_file: bool) -> Result<String> {
    let out = Command::new("curl")
        .args(curl_args(url, allow_file)?)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .context("running curl (is it installed?)")?;
    if !out.status.success() {
        bail!("downloading {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn download(d: &Download, dest: &Path, allow_file: bool, spinner: Option<&ui::Spinner>) -> Result<()> {
    let mut child = Command::new("curl")
        .args(curl_args(&d.url, allow_file)?)
        .arg("--output")
        .arg(dest)
        .arg(&d.url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running curl (is it installed?)")?;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if let Some(sp) = spinner {
            let got = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
            let total = d.size.map(|t| format!(" / {}", ui::human_bytes(t))).unwrap_or_default();
            sp.set_detail(&format!("{}  {}{total}", d.name, ui::human_bytes(got)));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut e, &mut err);
        }
        bail!("downloading {} failed: {}", d.url, err.trim());
    }
    let got = sha256_file(dest)?;
    if got != d.sha256 {
        let _ = fs::remove_file(dest);
        bail!("checksum mismatch for {}: expected {}, got {got}. Not installing it.", d.name, d.sha256);
    }
    Ok(())
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[derive(Serialize)]
struct Marker<'a> {
    tool: Tool,
    version: String,
    downloads: Vec<(&'a str, &'a str)>,
}

/// A toolchain ready to mount into a build.
#[derive(Debug, Clone)]
pub struct Ready {
    pub tool: Tool,
    pub version: Version,
    pub dir: PathBuf,
}

impl Ready {
    pub fn bin_dir(&self) -> PathBuf {
        self.dir.join("bin")
    }
    pub fn env(&self) -> Vec<(String, String)> {
        match self.tool {
            // Never let `go` try to fetch another toolchain behind our back.
            Tool::Go => vec![("GOTOOLCHAIN".into(), "local".into())],
            Tool::Rust => vec![],
        }
    }
}

/// Download (unless cached), verify and unpack a toolchain. Atomic: the
/// directory appears only once it is complete and verified.
pub fn provision(paths: &Paths, plan: &Plan, spinner: Option<&ui::Spinner>) -> Result<Ready> {
    let ready = Ready { tool: plan.tool, version: plan.version.clone(), dir: plan.dir.clone() };
    if plan.cached {
        return Ok(ready);
    }
    let root = toolchains_dir(paths);
    fs::create_dir_all(&root)?;
    let tmp = root.join(format!(".tmp-{}-{}", std::process::id(), plan.version));
    let _ = rm_rf(&tmp);
    fs::create_dir_all(&tmp)?;
    let result = (|| -> Result<PathBuf> {
        let allow_file = match plan.tool {
            Tool::Go => std::env::var("FORGE_GO_DL").is_ok_and(|b| b.starts_with("file://")),
            Tool::Rust => std::env::var("FORGE_RUST_DIST").is_ok_and(|b| b.starts_with("file://")),
        };
        let unpack = tmp.join("unpack");
        fs::create_dir_all(&unpack)?;
        for d in &plan.downloads {
            let file = tmp.join(&d.name);
            download(d, &file, allow_file, spinner)?;
            if let Some(sp) = spinner {
                sp.set_detail(&format!("unpacking {}", d.name));
            }
            let flag = if d.name.ends_with(".tar.xz") { "-xJf" } else { "-xzf" };
            run(Command::new("tar").args([flag]).arg(&file).arg("-C").arg(&unpack).arg("--no-same-owner"), "tar")?;
            fs::remove_file(&file)?;
        }
        let tree = match plan.tool {
            Tool::Go => unpack.join("go"),
            Tool::Rust => {
                // Each component unpacks to <name>-<ver>-<target>/<part>/...; the parts
                // listed in its `components` file are merged into one prefix.
                let tree = tmp.join("tree");
                fs::create_dir_all(&tree)?;
                for e in fs::read_dir(&unpack)? {
                    let comp = e?.path();
                    let list = fs::read_to_string(comp.join("components")).with_context(|| format!("{} has no components file", comp.display()))?;
                    for part in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
                        if part.contains('/') || part.contains("..") {
                            bail!("unexpected component name '{part}'");
                        }
                        let from = comp.join(part).join(".");
                        run(Command::new("cp").arg("-a").arg(&from).arg(&tree), "cp")?;
                    }
                }
                tree
            }
        };
        let probe = match plan.tool {
            Tool::Go => ["go"],
            Tool::Rust => ["rustc"],
        };
        for p in probe {
            if !tree.join("bin").join(p).is_file() {
                bail!("the unpacked {} toolchain has no bin/{p}", plan.tool.name());
            }
        }
        if plan.tool == Tool::Rust && !tree.join("bin/cargo").is_file() {
            bail!("the unpacked rust toolchain has no bin/cargo");
        }
        let marker = Marker {
            tool: plan.tool,
            version: plan.version.to_string(),
            downloads: plan.downloads.iter().map(|d| (d.url.as_str(), d.sha256.as_str())).collect(),
        };
        fs::write(tree.join(".forge-toolchain.json"), serde_json::to_vec_pretty(&marker)?)?;
        Ok(tree)
    })();
    let outcome = result.and_then(|tree| {
        match fs::rename(&tree, &plan.dir) {
            Ok(()) => Ok(()),
            // Another forge finished the same toolchain first: fine.
            Err(_) if plan.dir.join(".forge-toolchain.json").is_file() => Ok(()),
            Err(e) => Err(e).with_context(|| format!("moving the toolchain to {}", plan.dir.display())),
        }
    });
    let _ = rm_rf(&tmp);
    outcome?;
    Ok(ready)
}

/// `go-1.27.3`, `rust-1.99.1`
pub fn remove(paths: &Paths, name: &str) -> Result<()> {
    let known = installed(paths);
    let Some(i) = known.iter().find(|i| i.name == name) else {
        bail!("no toolchain named '{name}' (see `forge toolchain list`)");
    };
    // The name was matched against our own directory listing, so this is a
    // direct child of the toolchains directory.
    if i.dir.parent() != Some(toolchains_dir(paths).as_path()) {
        bail!("refusing to delete {}", i.dir.display());
    }
    rm_rf(&i.dir).with_context(|| format!("deleting {}", i.dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn versions() {
        assert_eq!(v("go1.27.3"), v("1.27.3"));
        assert_eq!(v("1.27"), v("1.27.0"), "1.27 and 1.27.0 are the same");
        assert!(v("1.27.3") > v("1.27"));
        assert!(v("1.9") < v("1.10"), "numeric, not textual");
        assert_eq!(v("1.99.1 (58cf8f9c3 2026-10-13)").to_string(), "1.99.1");
        assert_eq!(v("1.27rc1").to_string(), "1.27");
        assert_eq!(v("v2.0").to_string(), "2.0");
        for bad in ["", "go", "stable", "x.y"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn go_mod_requirements() {
        let r = go_requirement("module x\n\ngo 1.24.0\n\ntoolchain go1.24.3 // pinned\nrequire a v1\n").unwrap();
        assert_eq!(r.min.unwrap().to_string(), "1.24.3");
        assert!(r.why.contains("toolchain go1.24.3"), "{}", r.why);
        let r = go_requirement("module x\ngo 1.21\n").unwrap();
        assert_eq!(r.min.unwrap().to_string(), "1.21");
        // The `go` line may be older than `toolchain`; the higher wins, either order.
        let r = go_requirement("toolchain go1.22.5\ngo 1.21\n").unwrap();
        assert_eq!(r.min.unwrap().to_string(), "1.22.5");
        assert!(go_requirement("module x\n").unwrap().min.is_none());
    }

    #[test]
    fn rust_requirements() {
        let d = std::env::temp_dir().join(format!("forge-rustreq-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\nrust-version=\"1.80\"\n").unwrap();
        assert_eq!(rust_requirement(&d).unwrap().unwrap().min.unwrap().to_string(), "1.80");
        fs::write(d.join("Cargo.toml"), "[package]\nname=\"x\"\nedition=\"2024\"\n").unwrap();
        assert_eq!(rust_requirement(&d).unwrap().unwrap().min.unwrap().to_string(), "1.85");
        fs::write(d.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.90.0\"\n").unwrap();
        assert_eq!(rust_requirement(&d).unwrap().unwrap().min.unwrap().to_string(), "1.90.0");
        fs::write(d.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n").unwrap();
        assert_eq!(rust_requirement(&d).unwrap().unwrap().min.unwrap().to_string(), "1.85");
        fs::write(d.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"nightly-2025-01-01\"\n").unwrap();
        assert!(rust_requirement(&d).unwrap_err().to_string().contains("nightly"));
        fs::remove_dir_all(&d).unwrap();
    }

    const INDEX: &str = r#"[
      {"version":"go1.27.3","stable":true,"files":[
        {"filename":"go1.27.3.linux-amd64.tar.gz","os":"linux","arch":"amd64","kind":"archive","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":100},
        {"filename":"go1.27.3.linux-amd64.deb","os":"linux","arch":"amd64","kind":"installer","sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":1}]},
      {"version":"go1.28rc1","stable":false,"files":[
        {"filename":"go1.28rc1.linux-amd64.tar.gz","os":"linux","arch":"amd64","kind":"archive","sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","size":1}]},
      {"version":"go1.26.9","stable":true,"files":[
        {"filename":"go1.26.9.linux-amd64.tar.gz","os":"linux","arch":"amd64","kind":"archive","sha256":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","size":90}]}
    ]"#;

    #[test]
    fn picks_the_newest_stable_go_archive() {
        let (ver, d) = go_pick(INDEX, None, "amd64", "https://go.dev/dl/").unwrap();
        assert_eq!(ver.to_string(), "1.27.3", "release candidates are skipped");
        assert_eq!(d.url, "https://go.dev/dl/go1.27.3.linux-amd64.tar.gz");
        assert_eq!(d.sha256, "a".repeat(64));
        assert!(go_pick(INDEX, Some(&v("1.26")), "amd64", "x/").is_ok());
        let err = go_pick(INDEX, Some(&v("1.30")), "amd64", "x/").unwrap_err().to_string();
        assert!(err.contains("no stable Go release >= 1.30"), "{err}");
        assert!(go_pick(INDEX, None, "riscv64", "x/").is_err());
    }

    fn rust_manifest(avail: bool) -> String {
        let comp = |n: &str| {
            format!(
                "[pkg.{n}.target.x86_64-unknown-linux-gnu]\navailable = {avail}\nxz_url = \"https://static.rust-lang.org/dist/2026-10-15/{n}-1.99.1-x86_64-unknown-linux-gnu.tar.xz\"\nxz_hash = \"{}\"\n",
                "e".repeat(64)
            )
        };
        format!("[pkg.rust]\nversion = \"1.99.1 (58cf8f9c3 2026-10-13)\"\n{}{}{}", comp("rustc"), comp("cargo"), comp("rust-std"))
    }

    #[test]
    fn picks_the_rust_components() {
        let (ver, d) = rust_pick(&rust_manifest(true), "x86_64-unknown-linux-gnu", None).unwrap();
        assert_eq!(ver.to_string(), "1.99.1");
        assert_eq!(d.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), [
            "rustc-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
            "cargo-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
            "rust-std-1.99.1-x86_64-unknown-linux-gnu.tar.xz"
        ]);
        assert!(rust_pick(&rust_manifest(false), "x86_64-unknown-linux-gnu", None).is_err());
        assert!(rust_pick(&rust_manifest(true), "riscv64gc-unknown-linux-gnu", None).is_err());
        let err = rust_pick(&rust_manifest(true), "x86_64-unknown-linux-gnu", Some(&v("2.0"))).unwrap_err().to_string();
        assert!(err.contains("older than"), "{err}");
    }

    #[test]
    fn only_https_downloads_unless_a_local_override_is_in_use() {
        assert!(curl_args("https://go.dev/dl/x.tar.gz", false).is_ok());
        assert!(curl_args("http://go.dev/x", false).is_err());
        assert!(curl_args("file:///etc/passwd", false).is_err());
        assert!(curl_args("ftp://x/y", true).is_err());
        assert!(curl_args("file:///tmp/x", true).is_ok());
    }

    #[test]
    fn host_problem_messages() {
        let req = Requirement { tool: Tool::Go, min: Some(v("1.27")), why: "go.mod says `go 1.27.0`".into() };
        assert!(host_problem(&req, &[]).unwrap().contains("missing or doesn't run"));
    }
}
