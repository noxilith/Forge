//! Install and update, as a plan over one or more packages:
//!
//!   resolve (recipes, dependencies, commits) -> checkout -> one summary & confirmation
//!   -> build every package in dependency order (sandboxed, nothing installed yet)
//!   -> commit them all to the store in one critical section.
//!
//! If any build fails, nothing is installed or changed. A dependency that is
//! built in the same plan but not committed yet is mounted read-only into its
//! dependents' sandboxes at the path it will have in the store.

use anyhow::{Context, Result, bail};
use forge_build::{BuildConfig, DepMount, build, fetch, toolchain};
use forge_core::ui;
use forge_core::util::{deps_tag, rm_rf, unix_now, validate_name, validate_version_id};
use forge_core::{BuildSystem, DepRef, FileKind, Manifest, Paths, Recipe};
use forge_store::{NewPackage, Store};
use crate::taps;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const BUILD_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Clone)]
pub struct Flags {
    pub yes: bool,
    pub verbose: bool,
    pub sandbox: bool,
    pub keep_build: bool,
    /// Show every command that will run, not only the ones a recipe supplies.
    pub details: bool,
}

/// What to build, before it has been looked at.
struct Spec {
    name: String,
    repo: String,
    /// Revision to resolve to a commit.
    resolve_rev: Option<String>,
    /// Revision recorded in the manifest (what `update` tracks).
    store_rev: Option<String>,
    recipe: Option<Recipe>,
    origin: String,
    explicit: bool,
    replace: bool,
    system_override: Option<BuildSystem>,
    /// `--isolate` override for the recipe's `[run] isolate`.
    isolate: Option<String>,
    reason: Option<String>,
}

struct Node {
    name: String,
    repo: String,
    store_rev: Option<String>,
    explicit: bool,
    replace: bool,
    commit: String,
    version: String,
    /// `<version>-<commit8>`; gets a `-d<hash>` suffix when there are Forge dependencies.
    base_id: String,
    version_id: String,
    job_dir: PathBuf,
    recipe: Recipe,
    origin: String,
    system: BuildSystem,
    detected: bool,
    /// `(dependency name, build_only)`
    deps: Vec<(String, bool)>,
    reason: String,
    source_date_epoch: Option<i64>,
    /// Patch files from the recipe, applied to the unpacked source before building.
    patches: Vec<PathBuf>,
    /// A compiler Forge will provide for this build (and why the machine's won't do).
    toolchains: Vec<(toolchain::Plan, String)>,
}

impl Node {
    fn prefix(&self, paths: &Paths) -> PathBuf {
        paths.package_dir(&self.name, &self.version_id)
    }
    /// Where this node's package currently is, before it is committed.
    fn staged_prefix(&self, paths: &Paths) -> PathBuf {
        self.job_dir.join("stage").join(self.prefix(paths).strip_prefix("/").unwrap())
    }
}

struct Planner<'a> {
    paths: &'a Paths,
    flags: &'a Flags,
    installed: BTreeMap<String, Manifest>,
    pending: BTreeMap<String, Spec>,
    nodes: Vec<Node>,
    visiting: Vec<String>,
    synced: BTreeSet<String>,
    plan_dir: PathBuf,
    started: std::time::Instant,
}

impl<'a> Planner<'a> {
    fn new(paths: &'a Paths, flags: &'a Flags) -> Result<Self> {
        let installed = Store::open(paths)?.list()?.into_iter().map(|m| (m.name.clone(), m)).collect();
        let plan_dir = paths.builds().join(format!("plan-{}-{}", unix_now(), std::process::id()));
        std::fs::create_dir_all(&plan_dir)?;
        Ok(Planner {
            paths,
            flags,
            installed,
            pending: BTreeMap::new(),
            nodes: Vec::new(),
            visiting: Vec::new(),
            synced: BTreeSet::new(),
            plan_dir,
            started: std::time::Instant::now(),
        })
    }

    /// Fetch the repository's mirror (once per run) and resolve a revision.
    fn probe(&mut self, repo: &str, rev: Option<&str>) -> Result<(PathBuf, String)> {
        let mirror = fetch::mirror_path(self.paths, repo);
        if self.synced.insert(repo.to_string()) {
            let spinner = ui::Spinner::start(&format!("fetch {}", ui::short_url(repo)), true);
            let res = fetch::sync_mirror(&mirror, repo);
            spinner.finish(res.is_ok(), "");
            res?;
        }
        let commit = fetch::resolve(&mirror, rev)?;
        Ok((mirror, commit))
    }

    fn add(&mut self, spec: Spec) -> Result<()> {
        if self.nodes.iter().any(|n| n.name == spec.name) {
            return Ok(());
        }
        let reason = spec.reason.clone().unwrap_or_else(|| match self.visiting.last() {
            Some(parent) => format!("dependency of {parent}"),
            None => "requested".into(),
        });
        self.visiting.push(spec.name.clone());
        let node = self.prepare(spec, reason)?;
        for (dep, _) in node.deps.clone() {
            self.require(&dep)?;
        }
        self.visiting.pop();
        self.nodes.push(node);
        Ok(())
    }

    /// Make sure `name` is installed, or planned ahead of whoever needs it.
    fn require(&mut self, name: &str) -> Result<()> {
        if let Some(pos) = self.visiting.iter().position(|v| v == name) {
            let mut cycle: Vec<&str> = self.visiting[pos..].iter().map(String::as_str).collect();
            cycle.push(name);
            bail!("dependency cycle: {}", cycle.join(" -> "));
        }
        if self.nodes.iter().any(|n| n.name == name) {
            return Ok(());
        }
        if let Some(spec) = self.pending.remove(name) {
            return self.add(spec);
        }
        if self.installed.contains_key(name) {
            return Ok(());
        }
        let needed_by = self.visiting.last().cloned().unwrap_or_default();
        validate_name(name)?;
        let entry = taps::lookup(self.paths, name)?.with_context(|| {
            format!(
                "{needed_by} needs '{name}', which is not installed and has no recipe in the Forge package \
                 repository or in {} (try `forge sync` to refresh the repository)",
                self.paths.recipes().display()
            )
        })?;
        let recipe = entry.recipe.clone();
        let file = entry.file.clone();
        let repo = recipe
            .source_url()
            .map(str::to_string)
            .with_context(|| format!("{} has neither package.repo nor package.archive", file.display()))?;
        self.add(Spec {
            name: name.to_string(),
            repo,
            resolve_rev: recipe.package.rev.clone(),
            store_rev: recipe.package.rev.clone(),
            origin: entry.origin(),
            recipe: Some(recipe),
            explicit: false,
            replace: false,
            system_override: None,
            isolate: None,
            reason: None,
        })
    }

    fn prepare(&mut self, mut spec: Spec, reason: String) -> Result<Node> {
        validate_name(&spec.name)?;
        let job_dir = self.plan_dir.join(&spec.name);
        std::fs::create_dir_all(&job_dir)?;

        // Where the source comes from: a release tarball named by the recipe (checksummed),
        // or a git repository pinned to a commit.
        let archive = spec.recipe.as_ref().and_then(|r| {
            let p = &r.package;
            Some((p.archive.clone()?, p.sha256.clone()?, p.version.clone()?, p.strip_components.unwrap_or(1)))
        });
        let (mirror, commit, version, base_id, source_date_epoch);
        if let Some((url, sha, ver, strip)) = archive {
            let file_label = url.rsplit('/').next().unwrap_or("source").to_string();
            let spinner = ui::Spinner::start(&format!("fetch {file_label}"), true);
            let fetched = forge_build::archive::fetch(self.paths, &url, &sha, Some(&spinner));
            spinner.finish(fetched.is_ok(), "");
            let file = fetched?;
            let spinner = ui::Spinner::start(&format!("unpack {}", spec.name), true);
            let res = forge_build::archive::unpack(&file, &job_dir.join("src"), strip);
            spinner.finish(res.is_ok(), "");
            res?;
            mirror = PathBuf::new();
            base_id = format!("{ver}-{}", &sha[..8]);
            commit = sha;
            version = ver;
            source_date_epoch = None;
        } else {
            let (m, c) = self.probe(&spec.repo, spec.resolve_rev.as_deref())?;
            if let Some(pin) = spec.recipe.as_ref().and_then(|r| r.package.commit.clone()) {
                if !c.starts_with(&pin.to_ascii_lowercase()) {
                    bail!(
                        "commit mismatch for {}: recipe pins {pin} but {} resolves to {c}",
                        spec.name,
                        spec.resolve_rev.as_deref().unwrap_or("HEAD")
                    );
                }
            }
            let v = fetch::version_for(&m, &c);
            let short = &c[..8];
            base_id = if v.contains(short) { v.clone() } else { format!("{v}-{short}") };
            let spinner = ui::Spinner::start(&format!("unpack {}", spec.name), true);
            let res = fetch::checkout(&m, &spec.repo, &c, &job_dir.join("src"));
            spinner.finish(res.is_ok(), "");
            res?;
            source_date_epoch = fetch::commit_time(&m, &c);
            mirror = m;
            commit = c;
            version = v;
        }
        let _ = &mirror;
        validate_version_id(&base_id)?;
        if spec.recipe.is_none() {
            let in_repo = job_dir.join("src/forge.toml");
            if in_repo.is_file() {
                spec.recipe = Some(Recipe::from_file(&in_repo)?);
                spec.origin = "forge.toml in the repository".into();
            }
        }
        let mut recipe = spec.recipe.take().unwrap_or_default();
        if let Some(i) = &spec.isolate {
            recipe.run.isolate = Some(i.clone());
        }
        recipe.run.isolation()?;
        let (system, detected) = pick_system(spec.system_override, &recipe, &job_dir.join("src"))?;

        let mut deps: Vec<(String, bool)> = recipe.package.deps.iter().map(|d| (d.clone(), false)).collect();
        for d in &recipe.build.deps {
            if !deps.iter().any(|(n, _)| n == d) {
                deps.push((d.clone(), true));
            }
        }
        if deps.iter().any(|(n, _)| n == &spec.name) {
            bail!("{} cannot depend on itself", spec.name);
        }
        let toolchains = self.pick_toolchains(&spec.name, system, &job_dir.join("src"), &recipe)?;
        let patches = recipe.patch_files()?;
        Ok(Node {
            name: spec.name,
            repo: spec.repo,
            store_rev: spec.store_rev,
            explicit: spec.explicit,
            replace: spec.replace,
            source_date_epoch,
            patches,
            commit,
            version,
            base_id,
            version_id: String::new(),
            job_dir,
            recipe,
            origin: spec.origin,
            system,
            detected,
            toolchains,
            deps,
            reason,
        })
    }

    /// Does this build need a compiler Forge must provide? The machine's own is
    /// used when it works and is new enough (policy: `FORGE_TOOLCHAINS`).
    fn pick_toolchains(
        &self,
        name: &str,
        system: BuildSystem,
        src: &Path,
        recipe: &Recipe,
    ) -> Result<Vec<(toolchain::Plan, String)>> {
        let dir = match &recipe.build.subdir {
            Some(s) => src.join(s),
            None => src.to_path_buf(),
        };
        let dirs = if self.flags.sandbox { forge_build::deps::sandbox_path_dirs() } else { forge_core::util::path_dirs() };
        let mut out = Vec::new();
        for req in toolchain::requirements(system, &dir, &recipe.build.tools)? {
            let problem = toolchain::host_problem(&req, &dirs);
            let why = match (toolchain::mode(), problem) {
                (toolchain::Mode::Host, Some(p)) => bail!(
                    "{name} needs {} ({}): {p}. Install it, or unset FORGE_TOOLCHAINS=host to let Forge provide one.",
                    req.tool.name(),
                    req.why
                ),
                (toolchain::Mode::Host, None) | (toolchain::Mode::Auto, None) => continue,
                (toolchain::Mode::Auto, Some(p)) => p,
                (toolchain::Mode::Managed, _) => "FORGE_TOOLCHAINS=managed".to_string(),
            };
            let plan = toolchain::resolve(self.paths, &req)
                .with_context(|| format!("finding a {} toolchain for {name}", req.tool.name()))?;
            out.push((plan, why));
        }
        Ok(out)
    }

    /// Fix every package's version id (it depends on its dependencies' ids).
    fn finish(mut self) -> Result<Plan<'a>> {
        for i in 0..self.nodes.len() {
            let mut pairs = Vec::new();
            for (d, _) in &self.nodes[i].deps {
                let vid = self.nodes[..i]
                    .iter()
                    .find(|n| &n.name == d)
                    .map(|n| n.version_id.clone())
                    .or_else(|| self.installed.get(d).map(|m| m.version_id.clone()))
                    .with_context(|| format!("internal error: dependency {d} unresolved"))?;
                pairs.push((d.clone(), vid));
            }
            let n = &mut self.nodes[i];
            n.version_id = if pairs.is_empty() {
                n.base_id.clone()
            } else {
                format!("{}-d{}", n.base_id, deps_tag(&pairs))
            };
            validate_version_id(&n.version_id)?;
        }
        Ok(Plan {
            paths: self.paths,
            flags: self.flags,
            nodes: self.nodes,
            installed: self.installed,
            plan_dir: self.plan_dir,
            started: self.started,
        })
    }
}

struct Plan<'a> {
    paths: &'a Paths,
    flags: &'a Flags,
    nodes: Vec<Node>,
    installed: BTreeMap<String, Manifest>,
    plan_dir: PathBuf,
    started: std::time::Instant,
}

impl Plan<'_> {
    /// Direct dependencies of node `idx` with the exact versions they will have.
    fn dep_refs(&self, idx: usize) -> Vec<DepRef> {
        self.nodes[idx]
            .deps
            .iter()
            .map(|(d, build_only)| DepRef {
                name: d.clone(),
                version_id: self.version_of(idx, d),
                build_only: *build_only,
            })
            .collect()
    }

    fn version_of(&self, before: usize, name: &str) -> String {
        self.nodes[..before]
            .iter()
            .find(|n| n.name == name)
            .map(|n| n.version_id.clone())
            .or_else(|| self.installed.get(name).map(|m| m.version_id.clone()))
            .unwrap_or_default()
    }

    /// Every Forge package visible to node `idx`: direct dependencies plus the
    /// runtime dependencies of those, transitively.
    fn closure(&self, idx: usize) -> Result<Vec<DepMount>> {
        fn visit(p: &Plan, idx: usize, name: &str, runtime: bool, out: &mut Vec<DepMount>) -> Result<()> {
            let (prefix, host, children): (PathBuf, PathBuf, Vec<String>) =
                if let Some(n) = p.nodes[..idx].iter().find(|n| n.name == name) {
                    (
                        n.prefix(p.paths),
                        n.staged_prefix(p.paths),
                        n.deps.iter().filter(|(_, b)| !b).map(|(d, _)| d.clone()).collect(),
                    )
                } else if let Some(m) = p.installed.get(name) {
                    let dir = p.paths.package_dir(&m.name, &m.version_id);
                    (
                        dir.clone(),
                        dir,
                        m.deps.iter().filter(|d| !d.build_only).map(|d| d.name.clone()).collect(),
                    )
                } else {
                    bail!("dependency {name} is neither installed nor planned");
                };
            let changed = match out.iter_mut().find(|d| d.name == name) {
                Some(e) if runtime && !e.runtime => {
                    e.runtime = true;
                    true
                }
                Some(_) => false,
                None => {
                    out.push(DepMount { name: name.to_string(), prefix, host, runtime });
                    true
                }
            };
            if changed {
                for c in children {
                    visit(p, idx, &c, runtime, out)?;
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        for (d, build_only) in &self.nodes[idx].deps {
            visit(self, idx, d, !build_only, &mut out)?;
        }
        Ok(out)
    }

    fn config(&self, idx: usize) -> Result<BuildConfig> {
        let n = &self.nodes[idx];
        Ok(BuildConfig {
            job_dir: n.job_dir.clone(),
            prefix: n.prefix(self.paths),
            system: n.system,
            recipe: n.recipe.clone(),
            sandbox: self.flags.sandbox,
            verbose: self.flags.verbose,
            timeout: BUILD_TIMEOUT,
            jobs: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2),
            source_date_epoch: n.source_date_epoch,
            deps: self.closure(idx)?,
            toolchains: Vec::new(),
            patches: n.patches.clone(),
            patch_strip: n.recipe.package.patch_strip.unwrap_or(1),
        })
    }

    /// What is about to happen, kept short. Commands a recipe supplies are
    /// always listed (that text is what to review); the standard commands of a
    /// build system only with `--details`.
    fn print_plan(&self, configs: &[BuildConfig]) {
        let deps = self.nodes.iter().filter(|n| !n.explicit && !n.replace).count();
        println!();
        println!(
            "{}",
            ui::bold(&format!(
                "{} {} package{}{}",
                if self.nodes.iter().all(|n| n.replace) { "Updating" } else { "Installing" },
                self.nodes.len(),
                if self.nodes.len() == 1 { "" } else { "s" },
                if deps > 0 { format!(" ({deps} {})", if deps == 1 { "dependency" } else { "dependencies" }) } else { String::new() }
            ))
        );
        let row = |label: &str, value: String| println!("      {} {}", ui::dim(&format!("{label:<9}")), value);
        for (n, cfg) in self.nodes.iter().zip(configs) {
            println!();
            println!("  {} {} {}", ui::bullet(), ui::bold(&n.name), ui::dim(&n.version_id));
            if n.reason != "requested" {
                row("why", n.reason.clone());
            }
            row("source", format!("{} @ {}", ui::short_url(&n.repo), &n.commit[..8]));
            // "auto-detected" already says the build system was guessed; only a recipe
            // that left it out needs the extra note.
            let system = if n.detected && n.origin != "auto-detected" {
                format!("{} (detected)", n.system.as_str())
            } else {
                n.system.as_str().to_string()
            };
            row("recipe", format!("{} {} {system}", n.origin, ui::dot()));
            if !n.deps.is_empty() {
                let list: Vec<String> =
                    n.deps.iter().map(|(d, b)| format!("{d}{}", if *b { " (build only)" } else { "" })).collect();
                row("needs", list.join(", "));
            }
            row("runtime", describe_run(&n.recipe.run, &self.paths.root().join("data").join(&n.name)));
            for (t, why) in &n.toolchains {
                let how = if t.cached {
                    "already downloaded".to_string()
                } else if t.download_bytes() > 0 {
                    format!("downloads {} from {}", ui::human_bytes(t.download_bytes()), t.tool.source())
                } else {
                    format!("downloads from {}", t.tool.source())
                };
                row("toolchain", format!("{} {} {how}, kept for later builds  {}", t.label(), ui::dot(), ui::dim(&format!("({why})"))));
            }

            let supplied: Vec<&String> = n
                .recipe
                .build
                .fetch
                .iter()
                .chain(&n.recipe.build.steps)
                .chain(&n.recipe.build.configure_sh)
                .chain(&n.recipe.install.post)
                .collect();
            if self.flags.details {
                for (phase, cmds) in forge_build::build::describe(cfg) {
                    for c in cmds {
                        row(phase, c.replace('\n', "\n                  "));
                    }
                }
            } else {
                for c in supplied {
                    row("runs", format!("{} {}", ui::dim("$"), c.replace('\n', "\n                  ")));
                }
            }
        }
        println!();
        if self.flags.sandbox {
            println!(
                "  {}  bubblewrap {} no network while building {} your home directory is hidden",
                ui::dim("sandbox"),
                ui::dot(),
                ui::dot()
            );
        } else {
            println!("  {}  {}", ui::dim("sandbox"), ui::red("NONE: builds can read and write anything you can"));
        }
        if !self.flags.details {
            println!("  {}", ui::dim("(--details lists every command)"));
        }
    }

    fn run(self) -> Result<()> {
        let n_nodes = self.nodes.len();
        let configs: Vec<BuildConfig> = (0..n_nodes).map(|i| self.config(i)).collect::<Result<_>>()?;

        // Host requirements for every package, reported together.
        let mut problems = String::new();
        for (n, cfg) in self.nodes.iter().zip(&configs) {
            let tools = forge_build::build::required_tools(cfg);
            let provided: Vec<&str> = n.toolchains.iter().flat_map(|(t, _)| t.tool.provides().iter().copied()).collect();
            let mut mt = forge_build::deps::missing_tools(&tools, self.flags.sandbox);
            mt.retain(|t| !provided.contains(&t.as_str()));
            let mp = forge_build::deps::missing_pkgconfig(&n.recipe.build.pkgconfig);
            for t in mt {
                problems += &format!("  {}: tool {t}\n", n.name);
            }
            for p in mp {
                problems += &format!("  {}: pkg-config {p}\n", n.name);
            }
        }
        if !problems.is_empty() {
            bail!(
                "missing build dependencies on this system:\n{problems}\
                 Install them with your distribution's package manager and try again."
            );
        }

        self.print_plan(&configs);
        if !self.flags.yes && !confirm("Proceed?")? {
            bail!("aborted");
        }

        // Compilers Forge provides: downloaded and verified now, after you agreed, and reused later.
        let mut configs = configs;
        let mut ready: BTreeMap<PathBuf, toolchain::Ready> = BTreeMap::new();
        let wanted: Vec<&toolchain::Plan> = self.nodes.iter().flat_map(|n| n.toolchains.iter().map(|(p, _)| p)).collect();
        if wanted.iter().any(|p| !p.cached) {
            println!("\n{}", ui::bold("Toolchains"));
        }
        for p in wanted {
            if ready.contains_key(&p.dir) {
                continue;
            }
            let r = if p.cached {
                toolchain::provision(self.paths, p, None)
            } else {
                let spinner = ui::Spinner::start(&format!("download {}", p.label()), !self.flags.verbose);
                let r = toolchain::provision(self.paths, p, Some(&spinner));
                spinner.finish(r.is_ok(), "");
                r
            };
            ready.insert(p.dir.clone(), r.with_context(|| format!("getting {}", p.label()))?);
        }
        for (i, n) in self.nodes.iter().enumerate() {
            configs[i].toolchains = n.toolchains.iter().map(|(p, _)| ready[&p.dir].clone()).collect();
        }

        // Phase 1: build everything. Nothing is installed yet.
        println!("\n{}", ui::bold("Building"));
        for (i, cfg) in configs.iter().enumerate() {
            let n = &self.nodes[i];
            for d in &cfg.deps {
                if !d.host.is_dir() {
                    bail!("dependency {} produced no package directory at {}", d.name, d.host.display());
                }
            }
            if n_nodes > 1 {
                println!("  {} {}", ui::bold(&n.name), ui::dim(&n.version_id));
            }
            if let Err(e) = build(cfg) {
                eprintln!(
                    "\n{} last lines of {}\n{}\n",
                    ui::dim("---"),
                    ui::tilde(&cfg.log()),
                    forge_build::build::log_tail(&cfg.log(), 30)
                );
                eprintln!("Nothing was installed. Build files kept in {}", ui::tilde(&self.plan_dir));
                return Err(e.context(format!("building {}", n.name)));
            }
        }

        // Phase 2: commit, in dependency order, under one lock.
        let commit_started = std::time::Instant::now();
        let mut store = Store::open(self.paths)?;
        let mut committed: Vec<String> = Vec::new();
        let mut done: Vec<Manifest> = Vec::new();
        for (i, n) in self.nodes.iter().enumerate() {
            let cfg = &configs[i];
            let np = NewPackage {
                name: n.name.clone(),
                version: n.version.clone(),
                version_id: n.version_id.clone(),
                repo: n.repo.clone(),
                rev_spec: n.store_rev.clone(),
                commit: n.commit.clone(),
                build_system: n.system.as_str().to_string(),
                deps: self.dep_refs(i),
                build_deps: forge_build::build::required_tools(cfg)
                    .into_iter()
                    .chain(n.recipe.build.pkgconfig.iter().map(|p| format!("pkgconfig:{p}")))
                    .chain(n.toolchains.iter().map(|(p, _)| format!("toolchain:{}-{}", p.tool.name(), p.version)))
                    .collect(),
                explicit: n.explicit,
                run: n.recipe.run.clone(),
                link_dir: link_dir(),
                recipe: Some(n.recipe.to_toml()?),
            };
            match store.install(np, &cfg.stage(), n.recipe.install.exports.as_deref(), n.replace) {
                Ok(m) => {
                    if !n.replace {
                        committed.push(n.name.clone());
                    }
                    done.push(m);
                }
                Err(e) => {
                    // Undo fresh installs from this plan; replaced packages cannot be rolled back.
                    for name in committed.iter().rev() {
                        if let Err(re) = store.remove_ignoring(name, false, &committed) {
                            eprintln!("warning: could not roll back {name}: {re:#}");
                        }
                    }
                    if self.nodes.iter().any(|n| n.replace) && !done.is_empty() {
                        eprintln!("warning: some packages were already replaced; run `forge verify`");
                    }
                    eprintln!("Build files kept in {}", self.plan_dir.display());
                    return Err(e.context(format!("installing {}", n.name)));
                }
            }
        }
        drop(store);
        println!("{}", ui::step_line(true, "install", "", commit_started.elapsed()));

        println!();
        for m in &done {
            let nfiles = m.files.iter().filter(|f| f.kind != FileKind::Dir).count();
            println!(
                "  {} Installed {} {}{}  {}",
                ui::check(),
                ui::bold(&m.name),
                m.version_id,
                if m.explicit { "" } else { " [dependency]" },
                ui::dim(&format!("{nfiles} files"))
            );
        }
        let commands: Vec<&str> = done.iter().flat_map(|m| m.executables.iter().map(String::as_str)).collect();
        if !commands.is_empty() {
            println!();
            match link_dir() {
                Some(dir) => println!(
                    "  {} {}  {}",
                    ui::dim("ready"),
                    ui::bold(&commands.join(", ")),
                    ui::dim(&format!("(linked into {}, already on your PATH)", ui::tilde(&dir)))
                ),
                None if !on_path(&self.paths.bin()) => println!(
                    "  Commands: {}\n  To run them, add {} to your PATH (see `forge env`),\n  or set FORGE_LINK_DIR to a writable directory that already is on it.",
                    commands.join(", "),
                    ui::tilde(&self.paths.bin())
                ),
                None => println!("  Commands: {}", ui::bold(&commands.join(", "))),
            }
        }
        println!("\n  {}", ui::dim(&format!("done in {}", ui::fmt_dur(self.started.elapsed()))));
        if !self.flags.keep_build {
            let _ = rm_rf(&self.plan_dir);
        }
        Ok(())
    }
}

pub struct InstallRequest {
    pub repo: String,
    pub name: String,
    pub rev_spec: Option<String>,
    pub recipe: Option<Recipe>,
    pub recipe_origin: String,
    pub system_override: Option<BuildSystem>,
    pub isolate: Option<String>,
}

/// `forge build <link>`: build one package from a git link (or a local repository).
pub fn install(paths: &Paths, req: InstallRequest, flags: &Flags) -> Result<()> {
    install_many(paths, vec![req], flags)
}

/// `forge install <name>...`: install packages by name from the Forge package repository
/// (and your own recipes), and nowhere else.
pub fn install_names(paths: &Paths, names: &[String], flags: &Flags, isolate: Option<String>) -> Result<()> {
    let mut reqs = Vec::new();
    for n in names {
        if n.contains("://") || n.starts_with("git@") || (Path::new(n).is_dir() && (n.contains('/') || n.starts_with('.'))) {
            bail!(
                "'{n}' is a link or a path, and `forge install` takes package names from the Forge package \\
                 repository. To build it from source, use `forge build {n}`"
            );
        }
        validate_name(n)?;
        let e = taps::lookup(paths, n)?.with_context(|| {
            format!(
                "there is no package named '{n}' in the Forge package repository (`forge search {n}` looks for similar \\
                 names, `forge sync` refreshes the repository). To build it from a git link, use `forge build <link>`"
            )
        })?;
        let repo = e
            .recipe
            .source_url()
            .map(str::to_string)
            .with_context(|| format!("{} has neither package.repo nor package.archive", e.file.display()))?;
        reqs.push(InstallRequest {
            repo,
            name: n.clone(),
            rev_spec: e.recipe.package.rev.clone(),
            recipe_origin: e.origin(),
            recipe: Some(e.recipe),
            system_override: None,
            isolate: isolate.clone(),
        });
    }
    install_many(paths, reqs, flags)
}

fn install_many(paths: &Paths, reqs: Vec<InstallRequest>, flags: &Flags) -> Result<()> {
    check_sandbox(flags)?;
    let mut todo = Vec::new();
    for req in reqs {
        validate_name(&req.name)?;
        let existing = Store::open(paths)?.get(&req.name)?;
        match existing {
            Some(m) if m.explicit => {
                println!("  {} {} is already installed {}", ui::check(), req.name, ui::dim(&format!("({}; `forge update {}` checks for a newer one)", m.version_id, req.name)));
            }
            Some(_) => {
                // Installed only as a dependency: keep it, and stop treating it as removable.
                Store::open(paths)?.set_explicit(&req.name, true)?;
                println!("{} {} was already installed as a dependency; it is now kept explicitly.", ui::check(), req.name);
            }
            None => todo.push(req),
        }
    }
    if todo.is_empty() {
        return Ok(());
    }
    let mut planner = Planner::new(paths, flags)?;
    // With a recipe in hand, check the host for everything the whole dependency
    // tree needs before downloading anything.
    for req in &todo {
        if let Some(r) = &req.recipe {
            if let Err(e) = preflight(paths, &req.name, r, &planner.installed, flags) {
                let _ = rm_rf(&planner.plan_dir);
                return Err(e);
            }
        }
    }
    let result = (|| {
        for req in todo {
            planner.add(Spec {
                name: req.name,
                repo: req.repo,
                resolve_rev: req.rev_spec.clone(),
                store_rev: req.rev_spec,
                recipe: req.recipe,
                origin: req.recipe_origin,
                explicit: true,
                replace: false,
                system_override: req.system_override,
                isolate: req.isolate,
                reason: None,
            })?;
        }
        Ok(())
    })();
    finish_plan(planner, result)
}

/// Update the named packages (all installed packages if `names` is empty),
/// rebuilding everything that depends on a package that changed.
pub fn update(paths: &Paths, names: Vec<String>, flags: &Flags, refresh_recipe: bool) -> Result<()> {
    check_sandbox(flags)?;
    let mut planner = Planner::new(paths, flags)?;
    let result = (|| -> Result<()> {
        let targets: Vec<String> = if names.is_empty() {
            planner.installed.keys().cloned().collect()
        } else {
            names
        };
        if targets.is_empty() {
            println!("nothing installed");
            return Ok(());
        }
        let mut changed: BTreeMap<String, Manifest> = BTreeMap::new();
        for t in &targets {
            let m = planner.installed.get(t).with_context(|| format!("{t} is not installed"))?.clone();
            // A package built from a release tarball is "up to date" when the repository's
            // recipe still names the checksum it was built from.
            if is_archive_package(&m) {
                match taps::lookup(paths, &m.name)? {
                    Some(e) if e.recipe.package.archive.is_some() => {
                        if e.recipe.package.sha256.as_deref() == Some(m.commit.as_str()) {
                            println!("  {} {} is up to date {}", ui::check(), m.name, ui::dim(&format!("({})", m.version_id)));
                        } else {
                            changed.insert(m.name.clone(), m);
                        }
                    }
                    _ => println!(
                        "  {} {} is no longer in the package repository, so there is nothing to update it from",
                        ui::warn(),
                        m.name
                    ),
                }
                continue;
            }
            let (_, commit) = planner.probe(&m.repo, m.rev_spec.as_deref())?;
            if commit == m.commit {
                println!("  {} {} is up to date {}", ui::check(), m.name, ui::dim(&format!("({})", m.version_id)));
            } else {
                changed.insert(m.name.clone(), m);
            }
        }
        // Everything built against a changed package must be rebuilt.
        let store = Store::open(paths)?;
        let mut rebuild: BTreeMap<String, Manifest> = BTreeMap::new();
        for c in changed.keys() {
            for d in store.dependents_transitive(c)? {
                if !changed.contains_key(&d) {
                    if let Some(m) = planner.installed.get(&d) {
                        rebuild.insert(d, m.clone());
                    }
                }
            }
        }
        drop(store);
        // By default an update rebuilds with the recipe saved at install time.
        // `--refresh-recipe` takes the current one from your recipes/taps if
        // one describes the same repository.
        let catalog = if refresh_recipe { taps::catalog(paths)? } else { Vec::new() };
        let spec_for = |m: &Manifest, pin_commit: bool, reason: &str| -> Result<Spec> {
            if is_archive_package(m) {
                // The repository's recipe defines the version (and holds the patches).
                let e = taps::lookup(paths, &m.name)?
                    .with_context(|| format!("{} is no longer in the package repository", m.name))?;
                let repo = e.recipe.source_url().map(str::to_string).unwrap_or_else(|| m.repo.clone());
                return Ok(Spec {
                    name: m.name.clone(),
                    repo,
                    resolve_rev: None,
                    store_rev: None,
                    origin: e.origin(),
                    recipe: Some(e.recipe),
                    explicit: m.explicit,
                    replace: true,
                    system_override: None,
                    isolate: None,
                    reason: Some(reason.to_string()),
                });
            }
            let fresh = if refresh_recipe { taps::find_by_url(&catalog, &m.repo) } else { None };
            Ok(Spec {
                name: m.name.clone(),
                repo: m.repo.clone(),
                resolve_rev: if pin_commit { Some(m.commit.clone()) } else { m.rev_spec.clone() },
                store_rev: m.rev_spec.clone(),
                recipe: match fresh {
                    Some(e) => Some(e.recipe.clone()),
                    None => m.recipe.as_deref().map(Recipe::parse).transpose()?,
                },
                origin: match fresh {
                    Some(e) => e.origin(),
                    None => "recipe saved at install time".into(),
                },
                explicit: m.explicit,
                replace: true,
                system_override: None,
                isolate: None,
                reason: Some(reason.to_string()),
            })
        };
        for m in changed.values() {
            planner.pending.insert(m.name.clone(), spec_for(m, false, "update")?);
        }
        for m in rebuild.values() {
            planner
                .pending
                .insert(m.name.clone(), spec_for(m, true, "rebuild: a dependency was updated")?);
        }
        while let Some(name) = planner.pending.keys().next().cloned() {
            let spec = planner.pending.remove(&name).unwrap();
            planner.add(spec)?;
        }
        Ok(())
    })();
    finish_plan(planner, result)
}

/// Was this package built from a release tarball (rather than a git commit)?
fn is_archive_package(m: &Manifest) -> bool {
    m.recipe.as_deref().and_then(|r| Recipe::parse(r).ok()).is_some_and(|r| r.package.archive.is_some())
}

/// Report every missing host tool or library for a recipe and all the Forge
/// packages it needs (that aren't installed yet), using only the recipes: no
/// network, no downloads. Complements the per-package check made once the
/// sources are unpacked, which also covers auto-detected builds.
fn preflight(paths: &Paths, name: &str, recipe: &Recipe, installed: &BTreeMap<String, Manifest>, flags: &Flags) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![(name.to_string(), recipe.clone())];
    let mut problems = String::new();
    while let Some((n, r)) = stack.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        let mut tools = r.build.tools.clone();
        if let Some(sys) = r.build.system.as_deref().and_then(|s| BuildSystem::parse(s).ok()) {
            tools.extend(forge_build::backends::base_tools(sys));
        }
        // Go and Rust may be provided by Forge itself, so their absence isn't a problem.
        let provided: &[&str] = if toolchain::mode() == toolchain::Mode::Host { &[] } else { &["go", "cargo", "rustc", "meson", "python3", "python"] };
        for t in forge_build::deps::missing_tools(&tools, flags.sandbox).into_iter().filter(|t| !provided.contains(&t.as_str())) {
            problems += &format!("  {n}: tool {t}\n");
        }
        for p in forge_build::deps::missing_pkgconfig(&r.build.pkgconfig) {
            problems += &format!("  {n}: pkg-config {p}\n");
        }
        for d in r.package.deps.iter().chain(&r.build.deps) {
            if !installed.contains_key(d) {
                if let Ok(Some(e)) = taps::lookup(paths, d) {
                    stack.push((d.clone(), e.recipe));
                }
            }
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    // Sorted so the report doesn't depend on traversal order.
    let mut lines: Vec<&str> = problems.lines().collect();
    lines.sort();
    bail!(
        "missing build dependencies on this system:\n{}\n\
         Install them with your distribution's package manager and try again.",
        lines.join("\n")
    )
}

fn finish_plan(planner: Planner, planned: Result<()>) -> Result<()> {
    let plan_dir = planner.plan_dir.clone();
    let keep = planner.flags.keep_build;
    if let Err(e) = planned {
        let _ = rm_rf(&plan_dir); // nothing was built; the checkouts are not useful
        return Err(e);
    }
    if planner.nodes.is_empty() {
        let _ = rm_rf(&plan_dir);
        return Ok(());
    }
    // A failed run keeps the plan directory (logs) and says so itself.
    planner.finish()?.run()?;
    if !keep {
        let _ = rm_rf(&plan_dir);
    }
    Ok(())
}

/// One line telling the user where the program will keep its files.
fn describe_run(run: &forge_core::RunSection, data: &Path) -> String {
    use forge_core::Isolation;
    let data_s = ui::tilde(data);
    let mut parts: Vec<String> = Vec::new();
    match run.isolation() {
        Ok(Isolation::Xdg) => parts.push(format!("config, data, cache and state kept in {data_s}/")),
        Ok(Isolation::Home) => parts.push(format!("HOME and XDG directories kept in {data_s}/")),
        _ => parts.push("environment left alone".to_string()),
    }
    if !run.env.is_empty() {
        parts.push(format!("sets {}", run.env.keys().cloned().collect::<Vec<_>>().join(", ")));
    }
    if !run.args.is_empty() {
        parts.push(format!("extra args: {}", run.args.join(" ").replace("{data}", &data_s)));
    }
    if !run.seed.is_empty() {
        parts.push(format!("default files: {}", run.seed.keys().cloned().collect::<Vec<_>>().join(", ")));
    }
    parts.join("; ")
}

fn check_sandbox(flags: &Flags) -> Result<()> {
    if flags.sandbox {
        forge_build::sandbox::check().map_err(anyhow::Error::msg)
    } else {
        eprintln!("warning: running WITHOUT the sandbox. The build can read and write anything you can.");
        Ok(())
    }
}

fn pick_system(over: Option<BuildSystem>, recipe: &Recipe, src: &Path) -> Result<(BuildSystem, bool)> {
    if let Some(s) = over {
        return Ok((s, false));
    }
    if let Some(s) = &recipe.build.system {
        return Ok((BuildSystem::parse(s)?, false));
    }
    if !recipe.build.steps.is_empty() {
        return Ok((BuildSystem::Custom, false));
    }
    let dir = match &recipe.build.subdir {
        Some(s) => src.join(s),
        None => src.to_path_buf(),
    };
    match BuildSystem::detect(&dir) {
        Some(s) => Ok((s, true)),
        None => bail!(
            "could not detect a build system. Write a recipe with `[build] steps = [...]` \
             and pass it with --recipe, or add a forge.toml to the repository."
        ),
    }
}

/// A directory already on PATH that commands are also linked into, so they
/// work immediately without editing any shell config. `$FORGE_LINK_DIR`
/// overrides it (empty disables); otherwise `~/.local/bin` if it exists, is
/// on PATH and is writable.
pub fn link_dir() -> Option<PathBuf> {
    match std::env::var_os("FORGE_LINK_DIR") {
        Some(v) if v.is_empty() => return None,
        Some(v) => return Some(PathBuf::from(v)),
        None => {}
    }
    let dir = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin");
    let writable = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(dir.as_os_str()))
        .map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
        .unwrap_or(false);
    (dir.is_dir() && on_path(&dir) && writable).then_some(dir)
}

fn on_path(dir: &Path) -> bool {
    forge_core::util::path_dirs().iter().any(|p| p == dir)
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal: pass --yes to confirm non-interactively");
    }
    print!("{question} [Y/n] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    // Enter accepts, as with other package managers; anything but yes is a no.
    Ok(matches!(line.trim().to_lowercase().as_str(), "" | "y" | "yes"))
}

/// Interpret the user's `install` argument: a git URL, a local git repository,
/// or the name of a recipe. A URL that one of your recipes or taps describes
/// uses that recipe (unless `ignore_taps`), so installing niri by URL and by
/// name give the same result.
pub fn resolve_target(
    paths: &Paths,
    target: &str,
    recipe_file: Option<&PathBuf>,
    name: Option<&str>,
    ignore_taps: bool,
) -> Result<(String, Option<Recipe>, String, String)> {
    let mut recipe: Option<Recipe> = None;
    let mut origin = "auto-detected".to_string();
    let mut stem: Option<String> = None;
    if let Some(f) = recipe_file {
        recipe = Some(Recipe::from_file(f)?);
        origin = f.display().to_string();
    }

    let looks_like_url = target.contains("://") || target.starts_with("git@");
    let local = Path::new(target);
    let repo = if looks_like_url {
        if recipe.is_none() && !ignore_taps {
            let catalog = taps::catalog(paths)?;
            if let Some(e) = taps::find_by_url(&catalog, target) {
                println!(
                    "{} Using the recipe '{}' from {} for this repository {}",
                    ui::bullet(),
                    e.name,
                    e.origin(),
                    ui::dim("(--ignore-repo builds it without)")
                );
                recipe = Some(e.recipe.clone());
                origin = e.origin();
                stem = Some(e.name.clone());
            }
        }
        target.to_string()
    } else if local.is_dir() && (target.contains('/') || target.starts_with('.')) {
        local.canonicalize()?.to_string_lossy().into_owned()
    } else {
        bail!(
            "'{target}' is not a git link or a path to a repository. `forge build` builds from a link \
             (https://github.com/user/project); to install a package by name use `forge install {target}`"
        );
    };

    let name = name
        .map(str::to_string)
        .or_else(|| recipe.as_ref().and_then(|r| r.package.name.clone()))
        .or(stem)
        .unwrap_or_else(|| fetch::infer_name(&repo));
    Ok((repo, recipe, origin, name))
}
