//! How the everyday commands look: list, info, verify, remove, clean, search,
//! tap and doctor. Plain text when not on a terminal, colored on one (see
//! `forge_core::ui`).

use crate::{install, integrate, taps};
use anyhow::{Context, Result, bail};
use forge_core::util::{fmt_utc, found_in, path_dirs, rm_rf};
use forge_core::{FileKind, Manifest, Paths, ui};
use forge_store::Store;
use std::path::Path;

fn kv(label: &str, value: impl AsRef<str>) {
    println!("  {} : {}", ui::dim(&ui::pad(label, 12)), value.as_ref());
}

fn nfiles(m: &Manifest) -> usize {
    m.files.iter().filter(|f| f.kind != FileKind::Dir).count()
}

pub fn list(store: &Store) -> Result<()> {
    let pkgs = store.list()?;
    println!();
    if pkgs.is_empty() {
        println!("  no packages installed  {}", ui::dim("(try: forge install <git-url>)"));
        return Ok(());
    }
    let w = |f: fn(&Manifest) -> usize| pkgs.iter().map(f).max().unwrap_or(0);
    let (wn, wv) = (w(|m| m.name.chars().count()).max(4), w(|m| m.version_id.chars().count()).max(7));
    println!(
        "  {}",
        ui::dim(&format!("{}  {}  {}  {}  INSTALLED", ui::pad("NAME", wn), ui::pad("VERSION", wv), ui::pad("BUILD", 9), ui::pad("KIND", 10)))
    );
    for m in &pkgs {
        let kind = if m.explicit { ui::pad("explicit", 10) } else { ui::dim(&ui::pad("dependency", 10)) };
        println!(
            "  {}  {}  {}  {}  {}",
            ui::bold(&ui::pad(&m.name, wn)),
            ui::pad(&m.version_id, wv),
            ui::dim(&ui::pad(&m.build_system, 9)),
            kind,
            ui::dim(fmt_utc(m.installed_at).split(' ').next().unwrap_or(""))
        );
    }
    let deps = pkgs.iter().filter(|m| !m.explicit).count();
    println!(
        "\n  {}",
        ui::dim(&format!(
            "{} package{}{}",
            pkgs.len(),
            if pkgs.len() == 1 { "" } else { "s" },
            if deps > 0 { format!(", {deps} installed as dependencies") } else { String::new() }
        ))
    );
    Ok(())
}

pub fn info(paths: &Paths, store: &Store, name: &str) -> Result<()> {
    let m = store.get(name)?.with_context(|| format!("{name} is not installed"))?;
    println!();
    println!("  {} {} {}", ui::bullet(), ui::bold(&m.name), ui::dim(&m.version_id));
    println!();
    kv("Version", format!("{} ({})", m.version, m.version_id));
    kv("Repository", &m.repo);
    kv("Revision", m.rev_spec.as_deref().unwrap_or("(default branch)"));
    kv("Commit", &m.commit);
    kv("Build system", &m.build_system);
    kv("Installed", fmt_utc(m.installed_at));
    println!();
    kv("Location", ui::tilde(&paths.package_dir(&m.name, &m.version_id)));
    kv("Files", nfiles(&m).to_string());
    if !m.executables.is_empty() {
        kv("Executables", m.executables.join(", "));
    }
    for (l, t) in store.exports(&m.name)? {
        kv("Link", format!("{} -> {t}", l.display()));
    }
    println!();
    kv("Reason", if m.explicit { "installed explicitly" } else { "installed as a dependency" });
    let deps: Vec<String> = m
        .deps
        .iter()
        .map(|d| format!("{} {}{}", d.name, d.version_id, if d.build_only { " (build only)" } else { "" }))
        .collect();
    if !deps.is_empty() {
        kv("Forge deps", deps.join(", "));
    }
    let needed_by = store.dependents(&m.name)?;
    if !needed_by.is_empty() {
        kv("Required by", needed_by.join(", "));
    }
    if !m.build_deps.is_empty() {
        kv("Host deps", m.build_deps.join(", "));
    }
    println!();
    let iso = m.run.isolation().map(|i| format!("{i:?}").to_lowercase()).unwrap_or_default();
    let env = if m.run.env.is_empty() { String::new() } else { format!(" (+ env: {})", m.run.env.keys().cloned().collect::<Vec<_>>().join(", ")) };
    kv("Isolation", format!("{iso}{env}"));
    let data = store.data_dir(&m.name);
    kv("Data dir", format!("{}{}", ui::tilde(&data), if data.exists() { "" } else { " (not created yet)" }));
    for (p, _) in store.integrations(&m.name)? {
        kv("Integrated", p.display().to_string());
    }
    Ok(())
}

pub fn verify(store: &Store, package: Option<&str>) -> Result<()> {
    let problems = store.verify(package)?;
    println!();
    if problems.is_empty() {
        let n = match package {
            Some(_) => 1,
            None => store.list()?.len(),
        };
        if n == 0 {
            println!("  {} nothing installed, nothing to check", ui::check());
        } else {
            println!("  {} all files intact  {}", ui::check(), ui::dim(&format!("({n} package{} checked)", if n == 1 { "" } else { "s" })));
        }
        return Ok(());
    }
    let mut last = "";
    for (pkg, what) in &problems {
        if pkg != last {
            println!("  {} {}", ui::cross(), ui::bold(pkg));
            last = pkg;
        }
        println!("      {what}");
    }
    println!();
    bail!("{} problem(s) found", problems.len())
}

fn dir_size(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else { return 0 };
    if m.is_dir() {
        std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| dir_size(&e.path())).sum()
    } else {
        m.len()
    }
}

pub fn clean(paths: &Paths, all: bool) -> Result<()> {
    let _store = Store::open(paths)?; // hold the lock while deleting
    let mut parts = vec![("build leftovers", paths.builds()), ("trash", paths.trash())];
    if all {
        parts.push(("cached sources", paths.sources()));
    }
    println!();
    let (mut entries, mut bytes) = (0usize, 0u64);
    for (what, dir) in parts {
        let (mut n, mut b) = (0usize, 0u64);
        for e in std::fs::read_dir(&dir)? {
            let p = e?.path();
            b += dir_size(&p);
            rm_rf(&p)?;
            n += 1;
        }
        if n > 0 {
            println!("  {} {what}  {}", ui::check(), ui::dim(&format!("{n} item{}, {}", if n == 1 { "" } else { "s" }, ui::human_bytes(b))));
        }
        entries += n;
        bytes += b;
    }
    if entries == 0 {
        println!("  {} nothing to clean", ui::check());
    } else {
        println!("\n  Freed {}. Installed packages untouched.", ui::bold(&ui::human_bytes(bytes)));
    }
    if !all {
        println!("  {}", ui::dim("(--all also clears the cached sources)"));
    }
    Ok(())
}

pub fn search(paths: &Paths, query: &str) -> Result<()> {
    let q = query.to_lowercase();
    let hits: Vec<_> = taps::catalog(paths)?
        .into_iter()
        .filter(|e| {
            e.name.to_lowercase().contains(&q)
                || e.recipe.package.repo.as_deref().unwrap_or("").to_lowercase().contains(&q)
        })
        .collect();
    println!();
    if hits.is_empty() {
        println!("  no recipes match '{query}'  {}", ui::dim("(any git URL can still be installed directly)"));
        return Ok(());
    }
    let wn = hits.iter().map(|e| e.name.chars().count()).max().unwrap_or(0);
    let repos: Vec<String> = hits.iter().map(|e| ui::short_url(e.recipe.package.repo.as_deref().unwrap_or(""))).collect();
    let wr = repos.iter().map(|r| r.chars().count()).max().unwrap_or(0);
    for (e, repo) in hits.iter().zip(&repos) {
        println!("  {}  {}  {}", ui::bold(&ui::pad(&e.name, wn)), ui::dim(&ui::pad(repo, wr)), ui::cyan(&format!("[{}]", e.source)));
    }
    Ok(())
}

pub fn tap_list(paths: &Paths) -> Result<()> {
    let names = taps::tap_names(paths)?;
    println!();
    if names.is_empty() {
        println!("  no taps  {}", ui::dim("(add one with: forge tap add <git-url>)"));
        return Ok(());
    }
    let catalog = taps::catalog(paths)?;
    for n in names {
        let recipes: Vec<&str> = catalog.iter().filter(|e| e.source == n).map(|e| e.name.as_str()).collect();
        println!("  {} {}  {}", ui::bullet(), ui::bold(&n), ui::dim(&format!("{} recipe{}", recipes.len(), if recipes.len() == 1 { "" } else { "s" })));
        if !recipes.is_empty() {
            println!("      {}", recipes.join(", "));
        }
    }
    Ok(())
}

fn host_line(tool: forge_build::toolchain::Tool) -> (bool, String) {
    use forge_build::toolchain::host_version;
    match host_version(tool, &path_dirs()) {
        Some(v) => (true, format!("{} {v}", tool.name())),
        None => (false, format!("{} is missing or doesn't run", tool.name())),
    }
}

fn mode_name() -> &'static str {
    match forge_build::toolchain::mode() {
        forge_build::toolchain::Mode::Auto => "auto (use the machine's compiler if it works, else download)",
        forge_build::toolchain::Mode::Host => "host (never download)",
        forge_build::toolchain::Mode::Managed => "managed (always Forge's own)",
    }
}

pub fn toolchain_list(paths: &Paths) -> Result<()> {
    use forge_build::toolchain as tc;
    println!();
    println!("  {} {}  {}", ui::bullet(), ui::bold("Kept by Forge"), ui::dim(&format!("policy: {}", mode_name())));
    let kept = tc::installed(paths);
    if kept.is_empty() {
        println!("  {}", ui::dim("none yet; Forge downloads Go or Rust when a build needs one"));
    }
    for i in &kept {
        println!(
            "  {} {}  {}  {}",
            ui::check(),
            ui::bold(&ui::pad(&i.name, 14)),
            ui::pad(&ui::human_bytes(tc::dir_size(&i.dir)), 9),
            ui::dim(&ui::tilde(&i.dir))
        );
    }
    println!();
    println!("  {} {}", ui::bullet(), ui::bold("On this machine"));
    for t in [tc::Tool::Go, tc::Tool::Rust, tc::Tool::Meson, tc::Tool::Python] {
        let (ok, what) = host_line(t);
        println!("  {} {what}", if ok { ui::check() } else { ui::warn() });
    }
    Ok(())
}

pub fn toolchain_install(paths: &Paths, tool: &str, version: Option<&str>) -> Result<()> {
    use forge_build::toolchain as tc;
    let tool = tc::Tool::parse(tool)?;
    let min = match version {
        Some(v) => Some(tc::Version::parse(v).with_context(|| format!("'{v}' is not a version number"))?),
        None => None,
    };
    let plan = tc::resolve(paths, &tc::Requirement { tool, min, why: "requested".into() })?;
    println!();
    if plan.cached {
        println!("  {} {} is already downloaded", ui::check(), plan.label());
        return Ok(());
    }
    let spinner = ui::Spinner::start(&format!("download {}", plan.label()), true);
    let res = tc::provision(paths, &plan, Some(&spinner));
    spinner.finish(res.is_ok(), "");
    res?;
    println!("\n  Kept in {}. Builds use it automatically when the machine's {} isn't enough.", ui::tilde(&plan.dir), tool.name());
    Ok(())
}

pub fn toolchain_remove(paths: &Paths, name: &str) -> Result<()> {
    forge_build::toolchain::remove(paths, name)?;
    println!("\n  {} Removed {name}  {}", ui::check(), ui::dim("(downloaded again if a build needs it)"));
    Ok(())
}

pub fn relink(paths: &Paths, package: Option<String>) -> Result<()> {
    let Some(dir) = install::link_dir() else {
        bail!("no PATH directory to link into: ~/.local/bin is missing, not on PATH or not writable (set FORGE_LINK_DIR)");
    };
    let mut store = Store::open(paths)?;
    let names: Vec<String> = match package {
        Some(p) => vec![p],
        None => store.list()?.into_iter().map(|m| m.name).collect(),
    };
    println!();
    let mut total = 0;
    for n in names {
        for l in store.link_into(&n, &dir)? {
            println!("  {} linked {}", ui::check(), ui::tilde(&l));
            total += 1;
        }
    }
    println!("  {} link(s) created in {}", ui::bold(&total.to_string()), ui::tilde(&dir));
    Ok(())
}

pub fn doctor(paths: &Paths) -> Result<()> {
    let row = |ok: bool, what: &str, note: &str| {
        let mark = if ok { ui::check() } else { ui::warn() };
        println!("  {mark} {what}{}", if note.is_empty() { String::new() } else { format!("  {}", ui::dim(note)) });
    };
    println!();
    println!("  {} {}", ui::bullet(), ui::bold("Forge"));
    row(true, "root", &ui::tilde(paths.root()));
    row(unsafe { libc::geteuid() } != 0, "not running as root", "builds untrusted code, needs no privileges");
    let dirs = path_dirs();
    row(found_in(&dirs, "git"), "git", "");
    match forge_build::sandbox::check() {
        Ok(()) => row(true, "sandbox", "bubblewrap with user namespaces"),
        Err(e) => row(false, "sandbox", e.lines().next().unwrap_or("")),
    }
    match install::link_dir() {
        Some(d) => row(true, "commands get linked into", &format!("{} (on your PATH)", ui::tilde(&d))),
        None => row(
            dirs.iter().any(|d| d == &paths.bin()),
            "PATH",
            &format!("add {} to PATH (forge env), or set FORGE_LINK_DIR", ui::tilde(&paths.bin())),
        ),
    }
    println!();
    println!("  {} {}  {}", ui::bullet(), ui::bold("Compilers"), ui::dim(&format!("policy: {}", mode_name())));
    for t in [forge_build::toolchain::Tool::Go, forge_build::toolchain::Tool::Rust, forge_build::toolchain::Tool::Meson, forge_build::toolchain::Tool::Python] {
        let (ok, what) = host_line(t);
        let kept: Vec<String> = forge_build::toolchain::installed(paths).into_iter().filter(|i| i.tool == t).map(|i| i.name).collect();
        let note = if ok {
            String::new()
        } else if kept.is_empty() {
            "Forge downloads one when a build needs it".to_string()
        } else {
            format!("Forge has {}", kept.join(", "))
        };
        println!("  {} {what}{}", if ok { ui::check() } else { ui::warn() }, if note.is_empty() { String::new() } else { format!("  {}", ui::dim(&note)) });
    }
    println!();
    println!("  {} {}", ui::bullet(), ui::bold("Build tools"));
    let (have, missing): (Vec<&str>, Vec<&str>) =
        ["cargo", "cmake", "ninja", "make", "meson", "go", "pkg-config", "autoreconf", "cc"].into_iter().partition(|t| found_in(&dirs, t));
    println!("  {} {}", ui::check(), have.join(", "));
    if !missing.is_empty() {
        println!(
            "  {} {}  {}",
            ui::warn(),
            missing.join(", "),
            ui::dim("(only packages that use them need them)")
        );
    }
    Ok(())
}

/// Remove `names` in order (dependents first). `names` as a whole is the set
/// being removed, so members don't block each other.
pub fn remove_all(
    paths: &Paths,
    store: &mut Store,
    title: &str,
    names: &[String],
    dry_run: bool,
    yes: bool,
    keep_data: bool,
) -> Result<()> {
    // Things outside the package directory that will be deleted. Collected
    // first so one confirmation covers them, before anything is removed.
    let mut system_files = Vec::new();
    let mut data_dirs = Vec::new();
    for n in names {
        for (p, _) in store.integrations(n)? {
            system_files.push((n.clone(), p));
        }
        if !keep_data && store.data_has_files(n)? {
            data_dirs.push((n.clone(), store.data_dir(n)));
        }
    }
    println!();
    println!("{}", ui::bold(&if dry_run { format!("{title}  {}", ui::dim("(dry run: nothing is changed)")) } else { title.to_string() }));
    println!();
    if !dry_run && (!system_files.is_empty() || !data_dirs.is_empty()) {
        for (n, p) in &system_files {
            println!("  {} {n}: will remove system file {}", ui::warn(), p.display());
        }
        for (n, d) in &data_dirs {
            println!(
                "  {} {n}: will DELETE its data directory {} {}",
                ui::warn(),
                ui::tilde(d),
                ui::dim("(configuration and other files it created)")
            );
        }
        println!("  {}", ui::dim("(--keep-data keeps the data directory)"));
        if !yes && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            bail!("not a terminal: pass --yes to confirm");
        }
        if !yes {
            print!("\nContinue? [y/N] ");
            std::io::Write::flush(&mut std::io::stdout())?;
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)?;
            if !matches!(line.trim(), "y" | "Y" | "yes" | "Yes") {
                bail!("aborted");
            }
            println!();
        }
    }
    for (i, name) in names.iter().enumerate() {
        if dry_run {
            for (p, _) in store.integrations(name)? {
                println!("  {} Would remove system file {}", ui::dim("-"), p.display());
            }
        } else {
            for p in integrate::remove_integrations(paths, store, name)? {
                println!("  {} Removed system file {}", ui::check(), p.display());
            }
        }
        let r = store.remove_ignoring(name, dry_run, names)?;
        let verb = if dry_run { "Would remove" } else { "Removed" };
        let mark = if dry_run { ui::dim("-") } else { ui::check() };
        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        let facts = format!("{}, {}", plural(r.file_count, "file"), plural(r.removed_links.len(), "link"));
        println!(
            "  {mark} {verb} {} {}{}  {}",
            ui::bold(&r.name),
            r.version_id,
            if i > 0 { "  (no longer needed)" } else { "" },
            ui::dim(&facts)
        );
        for (l, why) in &r.skipped_links {
            println!("      {} left {} in place: {why}", ui::warn(), ui::tilde(l));
        }
        if dry_run {
            if !keep_data && store.data_dir(name).exists() {
                println!("      Would remove data directory {}", store.data_dir(name).display());
            }
        } else if !keep_data && store.remove_data(name)? {
            println!("      removed its data directory {}", ui::tilde(&store.data_dir(name)));
        }
    }
    println!();
    if dry_run {
        return Ok(());
    }
    if keep_data {
        println!("  {}", ui::dim("Data directories were kept."));
    }
    println!("  {}", ui::dim("Anything a program wrote outside Forge's directories is left alone."));
    Ok(())
}
