//! Source tarballs: download once, verify the SHA-256, keep in a cache, unpack
//! for each build.
//!
//! Like toolchain downloads this is `https` only, and the recipe must name the
//! checksum: a download that doesn't match it is deleted and the build stops.
//! (`file://` archives are accepted only when `FORGE_ALLOW_FILE_SOURCES=1`, for tests.)

use crate::toolchain::{Download, download_verified, sha256_file};
use anyhow::{Context, Result, bail};
use forge_core::{Paths, ui};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn allow_file() -> bool {
    std::env::var("FORGE_ALLOW_FILE_SOURCES").is_ok_and(|v| v == "1")
}

/// The file name of the archive: the last path segment of the URL, made safe.
fn file_name(url: &str) -> String {
    let last = url.split(['?', '#']).next().unwrap_or(url).rsplit('/').next().unwrap_or("source");
    let clean: String = last.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-') { c } else { '_' }).collect();
    let clean = clean.trim_start_matches('.');
    if clean.is_empty() { "source".into() } else { clean.to_string() }
}

/// `sources/archives/<sha256>-<file name>`. Named by checksum, so two recipes
/// that use the same tarball share one download.
pub fn cache_path(paths: &Paths, url: &str, sha256: &str) -> PathBuf {
    paths.sources().join("archives").join(format!("{sha256}-{}", file_name(url)))
}

/// Make sure the archive is in the cache and matches `sha256`; return its path.
pub fn fetch(paths: &Paths, url: &str, sha256: &str, spinner: Option<&ui::Spinner>) -> Result<PathBuf> {
    if url.starts_with("file://") && !allow_file() {
        bail!("refusing a file:// source");
    }
    let dest = cache_path(paths, url, sha256);
    if dest.is_file() && sha256_file(&dest).is_ok_and(|h| h == sha256) {
        return Ok(dest);
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    let part = dest.with_extension(format!("part-{}", std::process::id()));
    let _ = fs::remove_file(&part);
    let d = Download { name: file_name(url), url: url.to_string(), sha256: sha256.to_string(), size: None };
    download_verified(&d, &part, allow_file(), spinner).with_context(|| format!("fetching {url}"))?;
    fs::rename(&part, &dest)?;
    Ok(dest)
}

/// Unpack `file` into the (new or empty) directory `dest`, dropping `strip` leading directories.
pub fn unpack(file: &Path, dest: &Path, strip: u32) -> Result<()> {
    fs::create_dir_all(dest)?;
    let out = Command::new("tar")
        .arg("-xf")
        .arg(file)
        .arg("-C")
        .arg(dest)
        .arg(format!("--strip-components={strip}"))
        .arg("--no-same-owner")
        .stdin(Stdio::null())
        .output()
        .context("running tar (is it installed?)")?;
    if !out.status.success() {
        bail!("unpacking {} failed: {}", file.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    if fs::read_dir(dest)?.next().is_none() {
        bail!("unpacking {} produced nothing (wrong strip_components?)", file.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("https://github.com/a/b/archive/v1.0.tar.gz"), "v1.0.tar.gz");
        assert_eq!(file_name("https://x/y/../../etc/passwd"), "passwd");
        assert_eq!(file_name("https://x/a b.tar.gz?token=1#frag"), "a_b.tar.gz");
        assert_eq!(file_name("https://x/"), "source");
        assert_eq!(file_name("https://x/.hidden"), "hidden");
    }

    #[test]
    fn cache_is_named_by_checksum() {
        let p = Paths::new("/r");
        assert_eq!(cache_path(&p, "https://x/a-1.tar.xz", "abc"), PathBuf::from("/r/sources/archives/abc-a-1.tar.xz"));
    }
}
