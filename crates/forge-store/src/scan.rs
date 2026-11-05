//! Walking a directory tree into `FileEntry`s, with the policy checks that
//! decide whether a staged build result may become a package.

use anyhow::{Context, Result, bail};
use forge_core::{FileEntry, FileKind};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Walk `root` and describe everything below it, sorted by path.
///
/// `policy = Some(final_prefix)` is used for freshly built packages and
/// additionally rejects setuid/setgid bits, reserved `.forge` entries and
/// symlinks that point outside the package. With `None` (verification of an
/// installed package) the top-level `.forge` metadata directory is skipped.
pub fn scan_tree(root: &Path, policy: Option<&Path>) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let abs_dir = root.join(&rel_dir);
        for e in fs::read_dir(&abs_dir).with_context(|| format!("reading {}", abs_dir.display()))? {
            let e = e?;
            let rel = rel_dir.join(e.file_name());
            if rel.components().count() == 1 && e.file_name() == ".forge" {
                if policy.is_some() {
                    bail!("package contains a reserved '.forge' entry");
                }
                continue;
            }
            let abs = e.path();
            let meta = fs::symlink_metadata(&abs)?;
            let mode = meta.permissions().mode() & 0o7777;
            let rel_s = rel
                .to_str()
                .with_context(|| format!("non-UTF-8 file name in package: {}", abs.display()))?
                .to_string();
            let ft = meta.file_type();
            if ft.is_dir() {
                out.push(FileEntry { path: rel_s, kind: FileKind::Dir, mode, sha256: None, link_target: None });
                stack.push(rel);
            } else if ft.is_file() {
                if policy.is_some() && mode & 0o6000 != 0 {
                    bail!("refusing setuid/setgid file in package: {rel_s}");
                }
                out.push(FileEntry {
                    path: rel_s,
                    kind: FileKind::File,
                    mode,
                    sha256: Some(hash_file(&abs)?),
                    link_target: None,
                });
            } else if ft.is_symlink() {
                let target = fs::read_link(&abs)?;
                let target = target
                    .to_str()
                    .with_context(|| format!("non-UTF-8 symlink target in {rel_s}"))?
                    .to_string();
                if let Some(prefix) = policy {
                    check_symlink(rel.parent().unwrap_or(Path::new("")), &target, prefix)
                        .with_context(|| format!("symlink {rel_s} -> {target}"))?;
                }
                out.push(FileEntry {
                    path: rel_s,
                    kind: FileKind::Symlink,
                    mode: 0,
                    sha256: None,
                    link_target: Some(target),
                });
            } else {
                bail!("unsupported file type (device/socket/fifo) in package: {rel_s}");
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// A package symlink must stay inside the package: relative targets may not
/// climb out of it, absolute targets must lie under the package's final path.
fn check_symlink(rel_parent: &Path, target: &str, final_prefix: &Path) -> Result<()> {
    let t = Path::new(target);
    if t.is_absolute() {
        if t.components().any(|c| matches!(c, Component::ParentDir)) || !t.starts_with(final_prefix)
        {
            bail!("absolute symlink points outside the package");
        }
        return Ok(());
    }
    let mut depth = rel_parent.components().count() as i64;
    for c in t.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    bail!("relative symlink escapes the package");
                }
            }
            Component::CurDir => {}
            _ => bail!("unsupported symlink target"),
        }
    }
    Ok(())
}

/// A build installs into `stage/<final prefix>`. Anything staged *outside*
/// that path is something the build tried to put elsewhere on the system.
/// Returns the paths of such offenders (directories on the way to the prefix
/// are fine).
pub fn staged_outside_prefix(stage: &Path, prefix_in_stage: &Path) -> Result<Vec<String>> {
    let mut bad = Vec::new();
    let mut stack = vec![stage.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir)? {
            let e = e?;
            let p = e.path();
            if p == prefix_in_stage || p.starts_with(prefix_in_stage) {
                continue;
            }
            let meta = fs::symlink_metadata(&p)?;
            if prefix_in_stage.starts_with(&p) {
                if !meta.is_dir() {
                    bad.push(p.strip_prefix(stage).unwrap_or(&p).display().to_string());
                } else {
                    stack.push(p);
                }
            } else if meta.is_dir() {
                // Empty directories are harmless; files are not.
                collect_non_dirs(&p, stage, &mut bad)?;
            } else {
                bad.push(p.strip_prefix(stage).unwrap_or(&p).display().to_string());
            }
        }
    }
    bad.sort();
    Ok(bad)
}

fn collect_non_dirs(dir: &Path, stage: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if fs::symlink_metadata(&p)?.is_dir() {
            collect_non_dirs(&p, stage, out)?;
        } else {
            out.push(p.strip_prefix(stage).unwrap_or(&p).display().to_string());
        }
    }
    Ok(())
}
