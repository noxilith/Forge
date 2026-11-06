//! Prebuilt packages.
//!
//! A built package is stored as three files: `<platform>-<key>.tar.gz` (the package's files),
//! `<platform>-<key>.json` (what it is, with the tarball's SHA-256) and `<platform>-<key>.sig`
//! (an Ed25519 signature over the JSON). Forge only unpacks a tarball whose JSON carries a valid
//! signature from a key it trusts, whose SHA-256 matches, and which names exactly the package
//! it asked for. Anything else is ignored and the package is built from source instead.
//!
//! Compiled programs have their install path built in, so a binary only fits a machine with
//! the same `FORGE_ROOT`: the `key` covers the root, the platform, the package's version id
//! (which includes its dependencies' versions) and the full recipe and patches.

use crate::toolchain::sha256_file;
use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use forge_core::Paths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where prebuilt packages are looked for. `FORGE_BINARY_URL` overrides it (empty: never).
pub const DEFAULT_URL: &str = "https://github.com/noxilith/Forge-Packages/releases/download/binaries";

/// The official signing key (hex). Empty until one is generated with `forge binary keygen`.
pub const OFFICIAL_PUBKEY: &str = "b06947939f5122106df42741ec28be0a3d8fc54016c1f3e39badb4ec83972e50";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub key: String,
    pub name: String,
    pub version_id: String,
    pub root: String,
    pub platform: String,
    pub sha256: String,
    pub size: u64,
    /// No file mentions the install root, so the package works under any `FORGE_ROOT`
    /// (`root` is then empty and the key does not depend on it).
    #[serde(default)]
    pub relocatable: bool,
}

/// Everything a prebuilt package must match.
#[derive(Debug, Clone)]
pub struct Identity {
    pub root: PathBuf,
    pub name: String,
    pub version_id: String,
    /// The effective recipe, as saved in the package's manifest.
    pub recipe_toml: String,
    /// The recipe's patch files, in order.
    pub patches: Vec<PathBuf>,
}

pub fn platform() -> String {
    std::env::var("FORGE_PLATFORM").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| format!("{}-linux", std::env::consts::ARCH))
}

pub fn base_url() -> Option<String> {
    match std::env::var("FORGE_BINARY_URL") {
        Ok(v) if v.is_empty() => None,
        Ok(v) => Some(v.trim_end_matches('/').to_string()),
        Err(_) => Some(DEFAULT_URL.to_string()),
    }
}

/// Does any file under `dir` (or any symlink target) contain `needle`? Forge's own `.forge/` and
/// `.forge-run/` are skipped: they are not packed, and are made again wherever the package is installed.
fn mentions(dir: &Path, needle: &[u8]) -> Result<bool> {
    if needle.is_empty() {
        return Ok(false);
    }
    let has = |hay: &[u8]| -> bool {
        let first = needle[0];
        let mut i = 0;
        while let Some(p) = hay[i..].iter().position(|&b| b == first) {
            let at = i + p;
            if hay.len() - at < needle.len() {
                return false;
            }
            if &hay[at..at + needle.len()] == needle {
                return true;
            }
            i = at + 1;
        }
        false
    };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            if d == dir && (e.file_name() == ".forge" || e.file_name() == ".forge-run") {
                continue;
            }
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_symlink() {
                if has(std::os::unix::ffi::OsStrExt::as_bytes(fs::read_link(&p)?.as_os_str())) {
                    return Ok(true);
                }
            } else if ft.is_file() && has(&fs::read(&p)?) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

impl Identity {
    /// The same package for any root.
    pub fn relocatable(&self) -> Identity {
        Identity { root: PathBuf::new(), ..self.clone() }
    }

    pub fn key(&self) -> Result<String> {
        let mut h = Sha256::new();
        let mut put = |label: &str, v: &[u8]| {
            h.update(label.as_bytes());
            h.update((v.len() as u64).to_le_bytes());
            h.update(v);
        };
        put("format", b"forge-binary-1");
        put("platform", platform().as_bytes());
        put("root", self.root.to_string_lossy().as_bytes());
        put("name", self.name.as_bytes());
        put("version_id", self.version_id.as_bytes());
        put("recipe", self.recipe_toml.as_bytes());
        for p in &self.patches {
            let data = fs::read(p).with_context(|| format!("reading patch {}", p.display()))?;
            put("patch", &Sha256::digest(&data));
        }
        Ok(hex(&h.finalize()))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.is_ascii() {
        bail!("not a hex string");
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.into())).collect()
}

fn parse_pubkey(s: &str) -> Result<VerifyingKey> {
    let b: [u8; 32] = unhex(s)?.try_into().map_err(|_| anyhow::anyhow!("a public key is 32 bytes (64 hex digits)"))?;
    Ok(VerifyingKey::from_bytes(&b)?)
}

/// Keys whose signatures are accepted: the official one, `FORGE_BINARY_PUBKEY`, and every `*.pub`
/// file in `$FORGE_ROOT/binary-keys/` and `/etc/forge/binary-keys/` (a distribution's own key).
pub fn trusted_keys(paths: &Paths) -> Vec<VerifyingKey> {
    let mut texts: Vec<String> = Vec::new();
    if !OFFICIAL_PUBKEY.is_empty() {
        texts.push(OFFICIAL_PUBKEY.to_string());
    }
    if let Ok(k) = std::env::var("FORGE_BINARY_PUBKEY") {
        texts.push(k);
    }
    for dir in [paths.root().join("binary-keys"), PathBuf::from("/etc/forge/binary-keys")] {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.path().extension().is_some_and(|x| x == "pub") {
                if let Ok(t) = fs::read_to_string(e.path()) {
                    texts.push(t);
                }
            }
        }
    }
    texts.iter().filter_map(|t| parse_pubkey(t).ok()).collect()
}

/// Make a signing key pair: `<dir>/forge-binary.key` (secret, mode 0600) and `.pub`.
pub fn keygen(dir: &Path) -> Result<String> {
    fs::create_dir_all(dir)?;
    let mut seed = [0u8; 32];
    use std::io::Read;
    fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
    let sk = SigningKey::from_bytes(&seed);
    let public = hex(sk.verifying_key().as_bytes());
    let secret_path = dir.join("forge-binary.key");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&secret_path).with_context(|| format!("creating {} (it must not exist yet)", secret_path.display()))?;
        writeln!(f, "{}", hex(&seed))?;
    }
    fs::write(dir.join("forge-binary.pub"), format!("{public}\n"))?;
    Ok(public)
}

fn read_secret(path: &Path) -> Result<SigningKey> {
    let b: [u8; 32] = unhex(&fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("a secret key is 32 bytes (64 hex digits)"))?;
    Ok(SigningKey::from_bytes(&b))
}

/// Pack an installed package directory into `out`: the tarball, the JSON and the signature.
pub fn pack(id: &Identity, package_dir: &Path, out: &Path, secret: &Path) -> Result<Meta> {
    let sk = read_secret(secret)?;
    // Compiled programs often have their install path inside them; those only fit this root.
    // A package that never mentions it (a script, most Rust and Go programs) fits any root.
    let relocatable = !mentions(package_dir, id.root.to_string_lossy().as_bytes())?;
    let id = &if relocatable { id.relocatable() } else { id.clone() };
    let key = id.key()?;
    fs::create_dir_all(out)?;
    let stem = format!("{}-{key}", platform());
    let tar = out.join(format!("{stem}.tar.gz"));
    // Forge adds `.forge/` (the manifest) and `.forge-run/` (launchers) itself when it installs.
    let st = Command::new("tar")
        .args(["--sort=name", "--owner=0", "--group=0", "--numeric-owner", "--exclude=./.forge", "--exclude=./.forge-run", "-czf"])
        .arg(&tar)
        .arg("-C")
        .arg(package_dir)
        .arg(".")
        .status()
        .context("running tar")?;
    if !st.success() {
        bail!("tar failed packing {}", package_dir.display());
    }
    let meta = Meta {
        key,
        name: id.name.clone(),
        version_id: id.version_id.clone(),
        root: id.root.to_string_lossy().into_owned(),
        platform: platform(),
        sha256: sha256_file(&tar)?,
        size: fs::metadata(&tar)?.len(),
        relocatable,
    };
    let json = serde_json::to_vec_pretty(&meta)?;
    let sig: Signature = sk.sign(&json);
    fs::write(out.join(format!("{stem}.json")), &json)?;
    fs::write(out.join(format!("{stem}.sig")), format!("{}\n", hex(&sig.to_bytes())))?;
    Ok(meta)
}

fn curl(url: &str, dest: &Path) -> Result<bool> {
    let file_ok = std::env::var("FORGE_ALLOW_FILE_SOURCES").is_ok_and(|v| v == "1");
    if !url.starts_with("https://") && !(file_ok && url.starts_with("file://")) {
        bail!("prebuilt packages are fetched over https only");
    }
    let out = Command::new("curl")
        .args(["--silent", "--location", "--fail", "--max-time", "900", "--proto", if file_ok { "=https,file" } else { "=https" }, "--proto-redir", "=https", "--output"])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .context("running curl (is it installed?)")?;
    if out.status.success() {
        return Ok(true);
    }
    let _ = fs::remove_file(dest);
    // 22: the server answered 404 or another error status: "no such prebuilt package".
    if out.status.code() == Some(22) || out.status.code() == Some(37) {
        return Ok(false);
    }
    bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// A verified prebuilt package, ready to download.
pub struct Prebuilt {
    pub meta: Meta,
    tar_url: String,
}

/// Look for a prebuilt package: `Ok(None)` when there is none (or none can be trusted).
fn find_one(paths: &Paths, id: &Identity, work: &Path, relocatable: bool) -> Result<Option<Prebuilt>> {
    let Some(base) = base_url() else { return Ok(None) };
    let keys = trusted_keys(paths);
    if keys.is_empty() {
        return Ok(None);
    }
    let key = id.key()?;
    let stem = format!("{}-{key}", platform());
    fs::create_dir_all(work)?;
    let (jp, sp) = (work.join(format!("{stem}.json")), work.join(format!("{stem}.sig")));
    if !curl(&format!("{base}/{stem}.json"), &jp)? || !curl(&format!("{base}/{stem}.sig"), &sp)? {
        return Ok(None);
    }
    let json = fs::read(&jp)?;
    let sig = Signature::from_slice(&unhex(&fs::read_to_string(&sp)?)?).context("malformed signature")?;
    if !keys.iter().any(|k| k.verify(&json, &sig).is_ok()) {
        eprintln!("warning: the prebuilt {} is not signed by a trusted key; building from source", id.name);
        return Ok(None);
    }
    let meta: Meta = serde_json::from_slice(&json).context("malformed prebuilt metadata")?;
    let ok = meta.key == key
        && meta.name == id.name
        && meta.version_id == id.version_id
        && meta.platform == platform()
        && meta.relocatable == relocatable
        && Path::new(&meta.root) == id.root;
    if !ok {
        eprintln!("warning: the prebuilt {} does not match what was asked for; building from source", id.name);
        return Ok(None);
    }
    Ok(Some(Prebuilt { meta, tar_url: format!("{base}/{stem}.tar.gz") }))
}

impl Prebuilt {
    /// Download, check the SHA-256 and unpack into `dest` (the package's place in the stage).
    pub fn unpack_into(&self, dest: &Path, work: &Path) -> Result<()> {
        let tar = work.join(format!("{}.tar.gz", self.meta.key));
        if !curl(&self.tar_url, &tar)? {
            bail!("the prebuilt package for {} disappeared from the server", self.meta.name);
        }
        let got = sha256_file(&tar)?;
        if got != self.meta.sha256 {
            let _ = fs::remove_file(&tar);
            bail!("prebuilt {} has the wrong checksum (expected {}, got {got}); not using it", self.meta.name, self.meta.sha256);
        }
        crate::archive::unpack(&tar, dest, 0)?;
        let _ = fs::remove_file(&tar);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(root: &str, recipe: &str) -> Identity {
        Identity { root: root.into(), name: "p".into(), version_id: "1-abc".into(), recipe_toml: recipe.into(), patches: vec![] }
    }

    #[test]
    fn key_covers_root_recipe_and_patches() {
        let a = id("/r", "x").key().unwrap();
        assert_eq!(a, id("/r", "x").key().unwrap());
        assert_ne!(a, id("/other", "x").key().unwrap());
        assert_ne!(a, id("/r", "y").key().unwrap());
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.patch");
        fs::write(&p, "one").unwrap();
        let mut with = id("/r", "x");
        with.patches = vec![p.clone()];
        let k1 = with.key().unwrap();
        assert_ne!(a, k1);
        fs::write(&p, "two").unwrap();
        assert_ne!(k1, with.key().unwrap());
    }

    #[test]
    fn keygen_sign_and_verify_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let public = keygen(d.path()).unwrap();
        assert!(keygen(d.path()).is_err(), "never overwrite a secret key");
        let sk = read_secret(&d.path().join("forge-binary.key")).unwrap();
        let sig = sk.sign(b"data");
        let vk = parse_pubkey(&public).unwrap();
        assert!(vk.verify(b"data", &sig).is_ok());
        assert!(vk.verify(b"datb", &sig).is_err());
    }
}

/// Look for a prebuilt package made for this root, else one that fits any root.
pub fn find(paths: &Paths, id: &Identity, work: &Path) -> Result<Option<Prebuilt>> {
    if let Some(p) = find_one(paths, id, work, false)? {
        return Ok(Some(p));
    }
    find_one(paths, &id.relocatable(), work, true)
}

#[cfg(test)]
mod relocatable_tests {
    use super::*;

    fn id(root: &str) -> Identity {
        Identity { root: root.into(), name: "p".into(), version_id: "1-abc".into(), recipe_toml: "r".into(), patches: vec![] }
    }

    #[test]
    fn finds_the_root_in_files_and_links_but_not_elsewhere() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("bin")).unwrap();
        fs::write(d.path().join("bin/a"), b"\x7fELF....stuff").unwrap();
        assert!(!mentions(d.path(), b"/opt/forge/store/p/1").unwrap());
        fs::write(d.path().join("bin/b"), b"\x00\x01/opt/forge/store/p/1/lib\x00").unwrap();
        assert!(mentions(d.path(), b"/opt/forge/store/p/1").unwrap());
        fs::remove_file(d.path().join("bin/b")).unwrap();
        std::os::unix::fs::symlink("/opt/forge/store/p/1/x", d.path().join("bin/c")).unwrap();
        assert!(mentions(d.path(), b"/opt/forge/store").unwrap());
        assert!(!mentions(d.path(), b"").unwrap());
    }

    #[test]
    fn packs_a_pathless_package_for_any_root_and_a_path_baked_one_for_its_root() {
        let keys = tempfile::tempdir().unwrap();
        keygen(keys.path()).unwrap();
        let secret = keys.path().join("forge-binary.key");
        let out = tempfile::tempdir().unwrap();

        let plain = tempfile::tempdir().unwrap();
        fs::write(plain.path().join("tool"), "no paths in here").unwrap();
        let m = pack(&id("/home/me/forge"), plain.path(), out.path(), &secret).unwrap();
        assert!(m.relocatable && m.root.is_empty());
        assert_eq!(m.key, id("/anywhere/else").relocatable().key().unwrap());

        let baked = tempfile::tempdir().unwrap();
        fs::write(baked.path().join("lib"), "rpath=/home/me/forge/store/dep").unwrap();
        let m = pack(&id("/home/me/forge"), baked.path(), out.path(), &secret).unwrap();
        assert!(!m.relocatable && m.root == "/home/me/forge");
        assert_eq!(m.key, id("/home/me/forge").key().unwrap());
    }
}

#[cfg(test)]
mod skip_tests {
    use super::*;

    #[test]
    fn forges_own_directories_do_not_count() {
        let d = tempfile::tempdir().unwrap();
        for dir in [".forge", ".forge-run"] {
            fs::create_dir_all(d.path().join(dir)).unwrap();
            fs::write(d.path().join(dir).join("x"), "/opt/forge/store").unwrap();
        }
        assert!(!mentions(d.path(), b"/opt/forge").unwrap());
        fs::create_dir_all(d.path().join("share/.forge")).unwrap();
        fs::write(d.path().join("share/.forge/x"), "/opt/forge/store").unwrap();
        assert!(mentions(d.path(), b"/opt/forge").unwrap(), "only the top-level ones are skipped");
    }
}
