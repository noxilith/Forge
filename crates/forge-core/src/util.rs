use anyhow::{Result, bail};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn valid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')
}

fn validate_component(what: &str, s: &str, max: usize) -> Result<()> {
    if s.is_empty() || s.len() > max {
        bail!("invalid {what} '{s}': must be 1-{max} characters");
    }
    if !s.chars().all(valid_char) {
        bail!("invalid {what} '{s}': only letters, digits and . _ + - are allowed");
    }
    if !s.chars().next().unwrap().is_ascii_alphanumeric() || s.contains("..") {
        bail!("invalid {what} '{s}': must start with a letter or digit and not contain '..'");
    }
    Ok(())
}

/// Package names become directory names and are used in `rm -rf` paths,
/// so they are validated strictly.
pub fn validate_name(s: &str) -> Result<()> {
    validate_component("package name", s, 64)
}

pub fn validate_version_id(s: &str) -> Result<()> {
    validate_component("version", s, 128)
}

/// Turn arbitrary text (a git tag, say) into a valid version component.
pub fn sanitize_version(s: &str) -> String {
    let mapped: String = s.chars().map(|c| if valid_char(c) { c } else { '_' }).collect();
    let trimmed = mapped.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let out = trimmed.replace("..", "_");
    if out.is_empty() { "0".into() } else { out }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SS UTC`
pub fn fmt_utc(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Reduce the many spellings of a git remote to one comparable form, so
/// `https://github.com/A/B.git`, `git@github.com:a/b` and `ssh://git@github.com/a/b/`
/// are recognised as the same repository.
pub fn normalize_repo_url(url: &str) -> String {
    let mut s = url.trim().to_lowercase();
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.replacen(':', "/", 1);
    } else if let Some(i) = s.find("://") {
        s = s[i + 3..].to_string();
        // Drop credentials (`git@`, `user:token@`), but only before the first slash.
        let host_end = s.find('/').unwrap_or(s.len());
        if let Some(at) = s[..host_end].rfind('@') {
            s = s[at + 1..].to_string();
        }
    }
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let s = s.trim_end_matches('/');
    s.strip_prefix("www.").unwrap_or(s).to_string()
}

/// Short stable tag for a set of dependency versions; part of a package's
/// version id so that a changed dependency means a new store directory.
pub fn deps_tag(deps: &[(String, String)]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted = deps.to_vec();
    sorted.sort();
    let mut h = Sha256::new();
    for (n, v) in sorted {
        h.update(n.as_bytes());
        h.update(b"@");
        h.update(v.as_bytes());
        h.update(b"\n");
    }
    h.finalize().iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Single-quote a string for `sh`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Recursively delete a path (never following symlinks), fixing up read-only
/// directories (Go module caches, for instance) if needed.
pub fn rm_rf(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(m) if !m.is_dir() => return fs::remove_file(path),
        Ok(_) => {}
    }
    if fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    fn chmod_tree(p: &Path) {
        if let Ok(m) = fs::symlink_metadata(p) {
            if m.is_dir() {
                let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
                if let Ok(rd) = fs::read_dir(p) {
                    for e in rd.flatten() {
                        chmod_tree(&e.path());
                    }
                }
            }
        }
    }
    chmod_tree(path);
    fs::remove_dir_all(path)
}

/// Is `tool` an executable file in one of `dirs`?
pub fn found_in(dirs: &[PathBuf], tool: &str) -> bool {
    dirs.iter().any(|d| {
        fs::metadata(d.join(tool))
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("fastfetch").is_ok());
        assert!(validate_name("lib+x_1.2-a").is_ok());
        for bad in ["", "..", "../evil", "a/b", ".hidden", "-x", "a..b", "a b"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn repo_urls() {
        let canon = normalize_repo_url("https://github.com/hyprwm/Hyprland");
        assert_eq!(canon, "github.com/hyprwm/hyprland");
        for same in [
            "https://github.com/hyprwm/Hyprland.git",
            "https://github.com/hyprwm/Hyprland/",
            "http://www.github.com/hyprwm/hyprland.git/",
            "git@github.com:hyprwm/Hyprland.git",
            "ssh://git@github.com/hyprwm/Hyprland",
            "https://user:token@github.com/hyprwm/Hyprland",
            "  https://GitHub.com/hyprwm/Hyprland  ",
        ] {
            assert_eq!(normalize_repo_url(same), canon, "{same}");
        }
        assert_ne!(normalize_repo_url("https://github.com/hyprwm/hyprutils"), canon);
        assert_ne!(normalize_repo_url("https://gitlab.com/hyprwm/Hyprland"), canon);
        assert_eq!(normalize_repo_url("file:///tmp/x/repo"), "/tmp/x/repo");
    }

    #[test]
    fn time() {
        assert_eq!(fmt_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_version("v1.2/rc 1"), "v1.2_rc_1");
        assert_eq!(sanitize_version("../x"), "x");
    }
}
