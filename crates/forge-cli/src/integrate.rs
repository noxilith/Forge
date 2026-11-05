//! `forge integrate`: register a package's session files (compositors, window
//! managers) with the display manager.
//!
//! That needs files in root-owned directories, which the rest of Forge never
//! touches. Everything privileged happens here, in two operations (install a
//! file, delete a file), each shown to the user first and run through `sudo`
//! only when the directory isn't already writable. Every file is recorded with
//! its hash, so removal only deletes what Forge itself wrote.
//!
//! The session file is installed as a *copy* with `Exec=` pointing at a small
//! wrapper in `$FORGE_ROOT/integration/<pkg>/`. A copy, because the display
//! manager runs as another user that usually can't read your home directory;
//! a wrapper, because a login session's PATH doesn't contain Forge's `bin`.

use anyhow::{Context, Result, bail};
use forge_core::Paths;
use forge_core::desktop::{rewrite_session, safe_path_chars};
use forge_core::ui;
use forge_core::util::{rm_rf, sh_quote, validate_name};
use forge_store::{Store, hash_file};
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Planned {
    dest: PathBuf,
    wrapper: PathBuf,
    generated: PathBuf,
    content: String,
    target: String,
}

fn writable_dir(dir: &Path) -> bool {
    CString::new(dir.as_os_str().as_bytes())
        .map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
        .unwrap_or(false)
}

/// Where session files of this kind go. The default directories are fixed;
/// an override is honoured only if it needs no privileges, so an environment
/// variable can never redirect a `sudo` write.
fn session_dir(kind: &str) -> Result<PathBuf> {
    let (var, default) = match kind {
        "wayland" => ("FORGE_WAYLAND_SESSIONS_DIR", "/usr/share/wayland-sessions"),
        _ => ("FORGE_XSESSIONS_DIR", "/usr/share/xsessions"),
    };
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        if !p.is_absolute() || !p.is_dir() {
            bail!("{var} must be an existing absolute directory");
        }
        if !writable_dir(&p) {
            bail!("{var} must be writable without sudo");
        }
        return Ok(p);
    }
    let p = PathBuf::from(default);
    if !p.is_dir() {
        bail!("{default} does not exist; is a display manager installed?");
    }
    Ok(p)
}

fn run_sudo(args: &[&std::ffi::OsStr]) -> Result<()> {
    println!("  running: sudo {}", args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
    let status = Command::new("sudo")
        .args(args)
        .status()
        .context("running sudo (is it installed?). Run the command above as root to finish manually")?;
    if !status.success() {
        bail!("sudo command failed ({status})");
    }
    Ok(())
}

fn install_file(src: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().context("destination has no parent")?;
    if writable_dir(dir) {
        let tmp = dir.join(format!(".forge-{}.tmp", std::process::id()));
        fs::copy(src, &tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
        fs::rename(&tmp, dest)?;
        Ok(())
    } else {
        run_sudo(&["install".as_ref(), "-m".as_ref(), "0644".as_ref(), "-T".as_ref(), "--".as_ref(), src.as_os_str(), dest.as_os_str()])
    }
}

fn remove_file(dest: &Path) -> Result<()> {
    let dir = dest.parent().context("path has no parent")?;
    if writable_dir(dir) {
        fs::remove_file(dest)?;
        Ok(())
    } else {
        run_sudo(&["rm".as_ref(), "--".as_ref(), dest.as_os_str()])
    }
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal: pass --yes to confirm non-interactively");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

fn integration_dir(paths: &Paths, pkg: &str) -> PathBuf {
    paths.root().join("integration").join(pkg)
}

pub fn integrate(paths: &Paths, pkg: &str, dry_run: bool, yes: bool) -> Result<()> {
    validate_name(pkg)?;
    let mut store = Store::open(paths)?;
    let m = store.get(pkg)?.with_context(|| format!("{pkg} is not installed"))?;
    let pkgdir = paths.package_dir(&m.name, &m.version_id);
    let exported: Vec<String> = store
        .exports(pkg)?
        .iter()
        .filter_map(|(l, _)| l.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let idir = integration_dir(paths, pkg);

    // Resolve the command a session runs to something that survives updates:
    // Forge's `bin` link, or the package's `current` link.
    let mut resolve = |cmd: &str| -> Result<String> {
        let current = paths.current().join(pkg);
        if cmd.contains('/') {
            let p = Path::new(cmd);
            if let Ok(rel) = p.strip_prefix(&pkgdir) {
                return Ok(current.join(rel).to_string_lossy().into_owned());
            }
            if p.is_absolute() && !p.starts_with(paths.root()) {
                return Ok(cmd.to_string()); // a system program
            }
            bail!("session runs {cmd}, which is not inside the package");
        }
        if exported.iter().any(|e| e == cmd) {
            return Ok(paths.bin().join(cmd).to_string_lossy().into_owned());
        }
        if pkgdir.join("bin").join(cmd).is_file() {
            return Ok(current.join("bin").join(cmd).to_string_lossy().into_owned());
        }
        bail!(
            "the session file runs '{cmd}', which this package does not provide. \
             It probably expects a helper (or a systemd unit) the recipe doesn't install."
        )
    };

    let mut planned: Vec<Planned> = Vec::new();
    for (sub, kind) in [("share/wayland-sessions", "wayland"), ("share/xsessions", "x11")] {
        let dir = pkgdir.join(sub);
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "desktop") && e.path().is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();
        for name in names {
            let stem = name.trim_end_matches(".desktop");
            let wrapper = idir.join(format!("{stem}-session"));
            let src = fs::read_to_string(dir.join(&name)).with_context(|| format!("reading {sub}/{name}"))?;
            let r = rewrite_session(&src, &wrapper.to_string_lossy(), &mut resolve)
                .with_context(|| format!("{sub}/{name}"))?;
            planned.push(Planned {
                dest: session_dir(kind)?.join(format!("forge-{name}")),
                generated: idir.join(&name),
                wrapper,
                content: r.content,
                target: r.target,
            });
        }
    }
    if planned.is_empty() {
        bail!("{pkg} ships no session files (share/wayland-sessions/*.desktop or share/xsessions/*.desktop)");
    }
    if !safe_path_chars(&paths.root().to_string_lossy()) {
        bail!("{} contains characters that cannot be used safely in a session file", paths.root().display());
    }
    for p in &planned {
        match (fs::symlink_metadata(&p.dest), store.integration_owner(&p.dest)?) {
            (Ok(_), Some(o)) if o == pkg => {}
            (Ok(_), Some(o)) => bail!("{} belongs to package {o}", p.dest.display()),
            (Ok(_), None) => bail!("{} already exists and is not managed by Forge; refusing to overwrite it", p.dest.display()),
            (Err(_), _) => {}
        }
    }

    println!("Session files to register for {pkg}:");
    for p in &planned {
        println!("\n  {}  (root-owned directory)", p.dest.display());
        println!("  runs via wrapper {} -> {}", p.wrapper.display(), p.target);
        for l in p.content.lines() {
            println!("    | {l}");
        }
    }
    println!();
    if dry_run {
        return Ok(());
    }
    if !yes && !confirm("Install these files (may ask for your sudo password)?")? {
        bail!("aborted");
    }

    fs::create_dir_all(&idir)?;
    for p in &planned {
        let script = format!(
            "#!/bin/sh\n# Generated by Forge for package {pkg}. Do not edit.\nPATH={}:\"$PATH\"\nexport PATH\nexec {} \"$@\"\n",
            sh_quote(&paths.bin().to_string_lossy()),
            sh_quote(&p.target)
        );
        fs::write(&p.wrapper, script)?;
        fs::set_permissions(&p.wrapper, fs::Permissions::from_mode(0o755))?;
        fs::write(&p.generated, &p.content)?;
        let sha = hash_file(&p.generated)?;
        install_file(&p.generated, &p.dest)
            .with_context(|| format!("installing {}", p.dest.display()))?;
        store.record_integration(pkg, &p.dest, &sha)?;
        println!("  {} Installed {}", ui::check(), p.dest.display());
    }
    println!("Log out and pick the new entry (marked \"(Forge)\") in your display manager.");
    Ok(())
}

/// Delete the system files Forge placed for `pkg` (only if unchanged) and the
/// wrapper directory. Returns the files that were removed.
pub fn remove_integrations(paths: &Paths, store: &mut Store, pkg: &str) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for (path, sha) in store.integrations(pkg)? {
        match fs::symlink_metadata(&path) {
            Err(_) => {} // already gone
            Ok(m) if m.is_file() && hash_file(&path).is_ok_and(|h| h == sha) => {
                remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                removed.push(path.clone());
            }
            Ok(_) => eprintln!(
                "left {} in place: it is no longer the file Forge installed",
                path.display()
            ),
        }
        store.forget_integration(&path)?;
    }
    let dir = integration_dir(paths, pkg);
    if dir.parent() == Some(paths.root().join("integration").as_path()) {
        let _ = rm_rf(&dir);
        let _ = fs::remove_dir(paths.root().join("integration")); // only if empty
    }
    Ok(removed)
}

pub fn unintegrate(paths: &Paths, pkg: &str, yes: bool) -> Result<()> {
    validate_name(pkg)?;
    let mut store = Store::open(paths)?;
    let files = store.integrations(pkg)?;
    if files.is_empty() {
        println!("  {pkg} has no integrated files");
        return Ok(());
    }
    for (p, _) in &files {
        println!("Will remove {}", p.display());
    }
    if !yes && !confirm("Remove these files (may ask for your sudo password)?")? {
        bail!("aborted");
    }
    for p in remove_integrations(paths, &mut store, pkg)? {
        println!("  {} Removed {}", ui::check(), p.display());
    }
    Ok(())
}
