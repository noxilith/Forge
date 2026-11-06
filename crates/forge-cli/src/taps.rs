//! The Forge package repository, and other recipe repositories ("taps").
//!
//! `forge install <name>` takes packages from here and only from here: the
//! official repository (set up automatically on first use), your own recipes in
//! `$FORGE_ROOT/recipes/`, and any other repository you add with `forge tap add`.
//! Anything else is built from a git link with `forge build <url>`.
//!
//! A repository is a git repo with two recipe directories:
//! * `recipes/`: recipes written and kept up by hand
//! * `void/`: recipes generated from Void Linux's package templates (`forge import void`)
//!
//! A recipe is `<name>.toml`, or `<name>/recipe.toml` next to its `patches/`.
//! Lookup order: your own recipes, then each repository's `recipes/`, then its `void/`.
//! A git URL that a recipe describes uses that recipe, so `forge build
//! https://github.com/YaLTeR/niri` gives the same result as `forge install niri`.

use anyhow::{Context, Result, bail};
use forge_core::util::{normalize_repo_url, rm_rf, validate_name};
use forge_core::{Paths, Recipe, ui};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the official package repository lives. `FORGE_REPO` overrides it (empty: none).
pub const DEFAULT_REPO: &str = "https://github.com/noxilith/Forge-Packages";

/// Recipe directories inside a repository, in priority order.
const SUBDIRS: [&str; 2] = ["recipes", "void"];

pub struct Entry {
    /// `"local"`, or the repository's name (with `/void` for imported recipes).
    pub source: String,
    pub name: String,
    pub file: PathBuf,
    /// The recipe's path inside its repository, for display.
    pub rel: String,
    pub recipe: Recipe,
    /// Normalised source URL, for matching git URLs.
    pub repo_norm: String,
    /// Short commit of the repository this recipe came from.
    pub commit: Option<String>,
}

impl Entry {
    /// Human-readable origin recorded in plans.
    pub fn origin(&self) -> String {
        match (&*self.source, &self.commit) {
            ("local", _) => self.file.display().to_string(),
            (tap, Some(c)) => format!("'{tap}' ({} @ {c})", self.rel),
            (tap, None) => format!("'{tap}' ({})", self.rel),
        }
    }
}

pub fn taps_dir(paths: &Paths) -> PathBuf {
    paths.root().join("taps")
}

pub fn default_repo() -> Option<String> {
    match std::env::var("FORGE_REPO") {
        Ok(v) if v.is_empty() => None,
        Ok(v) => Some(v),
        Err(_) => Some(DEFAULT_REPO.to_string()),
    }
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut c = Command::new("git");
    c.args(args).env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let out = c.output().context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn tap_names(paths: &Paths) -> Result<Vec<String>> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir(taps_dir(paths)) {
        for e in rd.flatten() {
            if e.path().join(".git").exists() {
                if let Some(n) = e.file_name().to_str() {
                    v.push(n.to_string());
                }
            }
        }
    }
    v.sort();
    Ok(v)
}

/// `dir/<name>.toml` or `dir/<name>/recipe.toml`.
fn recipe_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let flat = dir.join(format!("{name}.toml"));
    if flat.is_file() {
        return Some(flat);
    }
    let nested = dir.join(name).join("recipe.toml");
    nested.is_file().then_some(nested)
}

/// Every recipe in `dir`: `(name, file)`, sorted.
fn list_recipes(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut v = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let Some(stem) = p.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        if p.is_file() && stem.ends_with(".toml") {
            v.push((stem.trim_end_matches(".toml").to_string(), p));
        } else if p.is_dir() && p.join("recipe.toml").is_file() {
            v.push((stem, p.join("recipe.toml")));
        }
    }
    v.sort();
    v
}

fn short_commit(dir: &Path) -> Option<String> {
    git(&["rev-parse", "--short=8", "HEAD"], Some(dir)).ok()
}

fn entry_from(source: &str, name: &str, file: PathBuf, rel: String, commit: Option<String>) -> Result<Entry> {
    let recipe = Recipe::from_file(&file)?;
    let repo_norm = recipe.source_url().map(normalize_repo_url).unwrap_or_default();
    Ok(Entry { source: source.to_string(), name: name.to_string(), file, rel, recipe, repo_norm, commit })
}

/// The recipe for package `name`, from your own recipes or the repositories (in priority
/// order). Looks the file up directly, so it stays fast however many recipes there are.
pub fn lookup(paths: &Paths, name: &str) -> Result<Option<Entry>> {
    validate_name(name)?;
    if let Some(f) = recipe_in(&paths.recipes(), name) {
        return entry_from("local", name, f, format!("recipes/{name}.toml"), None).map(Some);
    }
    let taps = tap_names(paths)?;
    for sub in SUBDIRS {
        for t in &taps {
            let root = taps_dir(paths).join(t);
            if let Some(f) = recipe_in(&root.join(sub), name) {
                let rel = f.strip_prefix(&root).map(|p| p.display().to_string()).unwrap_or_default();
                let source = if sub == "recipes" { t.clone() } else { format!("{t}/{sub}") };
                return entry_from(&source, name, f, rel, short_commit(&root)).map(Some);
            }
        }
    }
    Ok(None)
}

/// Add a repository. `announce` lists its recipes (turned off for big ones).
fn add_impl(paths: &Paths, source: &str, name: Option<&str>, announce: bool) -> Result<()> {
    let name = name.map(str::to_string).unwrap_or_else(|| forge_build::fetch::infer_name(source));
    validate_name(&name)?;
    let dest = taps_dir(paths).join(&name);
    if dest.exists() {
        bail!("a tap named '{name}' already exists (use --name to pick another, or `forge tap remove {name}`)");
    }
    fs::create_dir_all(taps_dir(paths))?;
    let spinner = ui::Spinner::start(&format!("clone {}", ui::short_url(source)), true);
    let d = dest.to_str().context("non-UTF-8 path")?;
    let cloned = git(&["clone", "--quiet", "--depth", "1", "--", source, d], None);
    spinner.finish(cloned.is_ok(), "");
    cloned?;
    let counts: Vec<(&str, usize)> = SUBDIRS.iter().map(|s| (*s, list_recipes(&dest.join(s)).len())).collect();
    let total: usize = counts.iter().map(|c| c.1).sum();
    if total == 0 {
        let _ = rm_rf(&dest);
        bail!("{source} has no recipes (expected recipes/<name>.toml or void/<name>.toml)");
    }
    println!();
    println!("  {} Added '{name}' with {total} recipes", ui::check());
    if announce && total <= 60 {
        let names: Vec<String> = SUBDIRS.iter().flat_map(|s| list_recipes(&dest.join(s))).map(|(n, _)| n).collect();
        println!("      {}", names.join(", "));
    } else {
        let parts: Vec<String> = counts.iter().filter(|c| c.1 > 0).map(|(s, n)| format!("{n} in {s}/")).collect();
        println!("      {}", parts.join(", "));
    }
    Ok(())
}

pub fn add(paths: &Paths, source: &str, name: Option<&str>) -> Result<()> {
    add_impl(paths, source, name, true)?;
    println!("  {}", ui::dim("Recipes are build instructions that run in the sandbox. Only add repositories you trust."));
    Ok(())
}

/// Set up the official package repository if it isn't there yet (first use).
pub fn ensure_default(paths: &Paths) -> Result<()> {
    let Some(url) = default_repo() else { return Ok(()) };
    let name = forge_build::fetch::infer_name(&url);
    if taps_dir(paths).join(&name).join(".git").exists() {
        return Ok(());
    }
    println!("{} Setting up the Forge package repository {}", ui::bullet(), ui::dim(&format!("({}, first use)", ui::short_url(&url))));
    add_impl(paths, &url, Some(&name), false).with_context(|| {
        format!("could not fetch the Forge package repository {url}; connect to the network, or set FORGE_REPO to another location")
    })
}

pub fn remove(paths: &Paths, name: &str) -> Result<()> {
    validate_name(name)?;
    let dir = taps_dir(paths).join(name);
    if !dir.join(".git").exists() {
        bail!("no tap named '{name}'");
    }
    rm_rf(&dir)?;
    println!("  {} Removed tap '{name}' {}", ui::check(), ui::dim("(packages installed from it are unaffected)"));
    Ok(())
}

pub fn update(paths: &Paths, only: Option<&str>) -> Result<()> {
    let names = match only {
        Some(n) => {
            validate_name(n)?;
            if !taps_dir(paths).join(n).join(".git").exists() {
                bail!("no tap named '{n}'");
            }
            vec![n.to_string()]
        }
        None => tap_names(paths)?,
    };
    println!();
    if names.is_empty() {
        println!("  no repositories  {}", ui::dim("(add one with: forge tap add <git-url>)"));
    }
    for n in names {
        let dir = taps_dir(paths).join(&n);
        let before = short_commit(&dir);
        let spinner = ui::Spinner::start(&format!("update {n}"), true);
        let res = git(&["fetch", "--quiet", "--depth", "1", "origin", "HEAD"], Some(&dir))
            .and_then(|_| git(&["reset", "--hard", "--quiet", "FETCH_HEAD"], Some(&dir)));
        let after = short_commit(&dir);
        let note = if before == after { "up to date".to_string() } else { format!("{} {} {}", before.unwrap_or_default(), ui::arrow(), after.unwrap_or_default()) };
        spinner.finish(res.is_ok(), &note);
        res.with_context(|| format!("updating '{n}'"))?;
    }
    Ok(())
}

/// Recipe counts per source: `(label, curated, imported)`.
pub fn counts(paths: &Paths) -> Result<Vec<(String, usize, usize)>> {
    let mut out = Vec::new();
    for t in tap_names(paths)? {
        let root = taps_dir(paths).join(&t);
        out.push((t, list_recipes(&root.join("recipes")).len(), list_recipes(&root.join("void")).len()));
    }
    Ok(out)
}

/// Every known recipe, in priority order. Scans everything: use `lookup` for a single name.
pub fn catalog(paths: &Paths) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut load = |source: &str, root: Option<&Path>, dir: &Path, commit: Option<String>| {
        for (name, file) in list_recipes(dir) {
            let rel = match root {
                Some(r) => file.strip_prefix(r).map(|p| p.display().to_string()).unwrap_or_default(),
                None => format!("recipes/{name}.toml"),
            };
            match entry_from(source, &name, file.clone(), rel, commit.clone()) {
                Ok(e) => out.push(e),
                Err(e) => eprintln!("warning: skipping {}: {e:#}", file.display()),
            }
        }
    };
    load("local", None, &paths.recipes(), None);
    let taps = tap_names(paths)?;
    for sub in SUBDIRS {
        for t in &taps {
            let root = taps_dir(paths).join(t);
            let source = if sub == "recipes" { t.clone() } else { format!("{t}/{sub}") };
            load(&source, Some(&root), &root.join(sub), short_commit(&root));
        }
    }
    Ok(out)
}


pub fn find_by_url<'a>(catalog: &'a [Entry], url: &str) -> Option<&'a Entry> {
    let n = normalize_repo_url(url);
    catalog.iter().find(|e| !e.repo_norm.is_empty() && e.repo_norm == n)
}
