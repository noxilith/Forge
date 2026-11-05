//! Git access via the `git` CLI: a bare mirror cache per repository, commit
//! resolution, and clean checkouts of a pinned commit.

use anyhow::{Context, Result, bail};
use forge_core::Paths;
use forge_core::util::sanitize_version;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut c = Command::new("git");
    c.args(args).env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let out = c.output().context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_opt(mirror: &Path, args: &[&str]) -> Option<String> {
    let mut full = vec!["-C", mirror.to_str()?];
    full.extend_from_slice(args);
    git(&full, None).ok().filter(|s| !s.is_empty())
}

/// `sources/<name>-<hash of url>.git`
pub fn mirror_path(paths: &Paths, url: &str) -> PathBuf {
    let h = Sha256::digest(url.as_bytes());
    let short: String = h.iter().take(6).map(|b| format!("{b:02x}")).collect();
    paths.sources().join(format!("{}-{short}.git", infer_name(url)))
}

/// `https://github.com/a/b.git` -> `b`
pub fn infer_name(url: &str) -> String {
    let t = url.trim_end_matches('/').trim_end_matches(".git");
    let last = t.rsplit(['/', ':']).next().unwrap_or(t);
    let s = sanitize_version(last);
    s
}

/// Create or refresh the bare mirror of `url`.
pub fn sync_mirror(mirror: &Path, url: &str) -> Result<()> {
    let m = mirror.to_str().context("non-UTF-8 path")?;
    if mirror.join("HEAD").exists() {
        git(&["-C", m, "remote", "update", "--prune"], None)
    } else {
        if let Some(p) = mirror.parent() {
            std::fs::create_dir_all(p)?;
        }
        // History without file contents: a fraction of the download. The files of
        // the one commit we build are fetched when it is checked out.
        git(&["clone", "--mirror", "--filter=blob:none", "--quiet", "--", url, m], None)
    }
    .map(|_| ())
    .with_context(|| format!("fetching {url}"))
}

/// Resolve a tag/branch/sha (or the default branch) to a full commit hash.
pub fn resolve(mirror: &Path, rev: Option<&str>) -> Result<String> {
    let m = mirror.to_str().context("non-UTF-8 path")?;
    let spec = match rev {
        None => "HEAD".to_string(),
        Some(r) => {
            if r.starts_with('-') || r.is_empty() {
                bail!("invalid revision '{r}'");
            }
            format!("{r}^{{commit}}")
        }
    };
    let sha = git(&["-C", m, "rev-parse", "--verify", "--end-of-options", &spec], None)
        .with_context(|| format!("resolving revision {}", rev.unwrap_or("HEAD")))?;
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("unexpected rev-parse output: {sha}");
    }
    Ok(sha)
}

/// A human-readable version: the tag if the commit is tagged, else the nearest
/// tag plus `+git<sha>`, else `0+git<sha>`.
pub fn version_for(mirror: &Path, commit: &str) -> String {
    let short = &commit[..8];
    let strip = |t: String| {
        let t = t.strip_prefix('v').filter(|r| r.starts_with(|c: char| c.is_ascii_digit())).map(str::to_string).unwrap_or(t);
        sanitize_version(&t)
    };
    if let Some(t) = git_opt(mirror, &["describe", "--tags", "--exact-match", commit]) {
        return strip(t);
    }
    if let Some(t) = git_opt(mirror, &["describe", "--tags", "--abbrev=0", commit]) {
        return format!("{}+git{short}", strip(t));
    }
    format!("0+git{short}")
}

/// Committer timestamp, used for SOURCE_DATE_EPOCH.
pub fn commit_time(mirror: &Path, commit: &str) -> Option<i64> {
    git_opt(mirror, &["show", "-s", "--format=%ct", commit])?.parse().ok()
}

/// Check out exactly `commit` into `dest` (detached, with submodules).
///
/// `origin_url` is where missing file contents are fetched from if the
/// mirror doesn't have them (it holds history only).
pub fn checkout(mirror: &Path, origin_url: &str, commit: &str, dest: &Path) -> Result<()> {
    let m = mirror.to_str().context("non-UTF-8 path")?;
    let d = dest.to_str().context("non-UTF-8 path")?;
    git(&["clone", "--quiet", "--no-checkout", "--filter=blob:none", "--", m, d], None)?;
    git(&["remote", "set-url", "origin", origin_url], Some(dest))?;
    git(&["-c", "advice.detachedHead=false", "checkout", "--quiet", "--detach", commit], Some(dest))?;
    if dest.join(".gitmodules").exists() {
        git(&["submodule", "update", "--init", "--recursive", "--depth", "1", "--quiet"], Some(dest))
            .context("fetching submodules")?;
    }
    // Defence in depth: the checked-out commit must be what we pinned.
    let head = git(&["rev-parse", "HEAD"], Some(dest))?;
    if head != commit {
        bail!("checkout is at {head}, expected {commit}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(infer_name("https://github.com/fastfetch-cli/fastfetch"), "fastfetch");
        assert_eq!(infer_name("https://github.com/a/b.git/"), "b");
        assert_eq!(infer_name("git@github.com:a/niri.git"), "niri");
        assert_eq!(infer_name("/home/me/src/proj"), "proj");
    }
}
