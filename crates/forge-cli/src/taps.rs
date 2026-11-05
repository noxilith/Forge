//! Recipe repositories ("taps"): git repos holding `recipes/<name>.toml`.
//!
//! Forge never needs one: any git URL installs on its own with
//! auto-detection. A tap is where packages that need dependencies or special
//! isolation settings live. Installing a git URL that a recipe describes uses
//! that recipe, so `forge install https://github.com/YaLTeR/niri` gives the
//! same result as `forge install niri`.
//!
//! Lookup order: your own `$FORGE_ROOT/recipes/` first, then taps in
//! alphabetical order.

use anyhow::{Context, Result, bail};
use forge_core::util::{normalize_repo_url, rm_rf, validate_name};
use forge_core::{Paths, Recipe, ui};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Entry {
    /// `"local"` or the tap's name.
    pub source: String,
    pub name: String,
    pub file: PathBuf,
    pub recipe: Recipe,
    /// Normalised `package.repo`, for matching git URLs.
    pub repo_norm: String,
    /// Short commit of the tap this recipe came from.
    pub commit: Option<String>,
}

impl Entry {
    /// Human-readable origin recorded in plans.
    pub fn origin(&self) -> String {
        match (&*self.source, &self.commit) {
            ("local", _) => self.file.display().to_string(),
            (tap, Some(c)) => format!("tap '{tap}' (recipes/{}.toml @ {c})", self.name),
            (tap, None) => format!("tap '{tap}' (recipes/{}.toml)", self.name),
        }
    }
}

pub fn taps_dir(paths: &Paths) -> PathBuf {
    paths.root().join("taps")
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

fn recipe_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml") && p.is_file())
        .collect();
    v.sort();
    v
}

fn short_commit(dir: &Path) -> Option<String> {
    git(&["rev-parse", "--short=8", "HEAD"], Some(dir)).ok()
}

pub fn add(paths: &Paths, source: &str, name: Option<&str>) -> Result<()> {
    let name = name.map(str::to_string).unwrap_or_else(|| forge_build::fetch::infer_name(source));
    validate_name(&name)?;
    let dest = taps_dir(paths).join(&name);
    if dest.exists() {
        bail!("a tap named '{name}' already exists (use --name to pick another, or `forge tap remove {name}`)");
    }
    fs::create_dir_all(taps_dir(paths))?;
    let spinner = ui::Spinner::start(&format!("clone {}", ui::short_url(source)), true);
    let d = dest.to_str().context("non-UTF-8 path")?;
    let cloned = git(&["clone", "--quiet", "--", source, d], None);
    spinner.finish(cloned.is_ok(), "");
    cloned?;
    let found = recipe_files(&dest.join("recipes"));
    if found.is_empty() {
        let _ = rm_rf(&dest);
        bail!("{source} has no recipes (expected recipes/<name>.toml)");
    }
    let names: Vec<String> = found.iter().map(|f| f.file_stem().unwrap().to_string_lossy().into_owned()).collect();
    println!();
    println!("  {} Added tap '{name}' with {} recipes", ui::check(), found.len());
    println!("      {}", names.join(", "));
    println!("  {}", ui::dim("Recipes are build instructions that run in the sandbox. Only add taps you trust."));
    Ok(())
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
        println!("  no taps  {}", ui::dim("(add one with: forge tap add <git-url>)"));
    }
    for n in names {
        let dir = taps_dir(paths).join(&n);
        let before = short_commit(&dir);
        git(&["fetch", "--quiet", "origin", "HEAD"], Some(&dir)).with_context(|| format!("updating tap '{n}'"))?;
        git(&["reset", "--hard", "--quiet", "FETCH_HEAD"], Some(&dir))?;
        let after = short_commit(&dir);
        println!(
            "  {} {n}  {}",
            ui::check(),
            ui::dim(&if before == after {
                "up to date".to_string()
            } else {
                format!("{} {} {}", before.unwrap_or_default(), ui::arrow(), after.unwrap_or_default())
            })
        );
    }
    Ok(())
}

/// Every known recipe, in priority order: your own first, then each tap.
pub fn catalog(paths: &Paths) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut load = |source: &str, dir: &Path, commit: Option<String>| {
        for file in recipe_files(dir) {
            let name = file.file_stem().unwrap().to_string_lossy().to_string();
            match Recipe::from_file(&file) {
                Ok(recipe) => {
                    let repo_norm = recipe.package.repo.as_deref().map(normalize_repo_url).unwrap_or_default();
                    out.push(Entry { source: source.to_string(), name, file, recipe, repo_norm, commit: commit.clone() });
                }
                Err(e) => eprintln!("warning: skipping {}: {e:#}", file.display()),
            }
        }
    };
    load("local", &paths.recipes(), None);
    for t in tap_names(paths)? {
        let dir = taps_dir(paths).join(&t);
        load(&t, &dir.join("recipes"), short_commit(&dir));
    }
    Ok(out)
}

pub fn find_by_name<'a>(catalog: &'a [Entry], name: &str) -> Option<&'a Entry> {
    catalog.iter().find(|e| e.name == name)
}

pub fn find_by_url<'a>(catalog: &'a [Entry], url: &str) -> Option<&'a Entry> {
    let n = normalize_repo_url(url);
    catalog.iter().find(|e| !e.repo_norm.is_empty() && e.repo_norm == n)
}
