//! The Forge store: ownership database, atomic install, verified removal.
//!
//! Rules this crate enforces:
//! * Forge only ever deletes (a) a package directory at the exact path
//!   `store/<name>/<version-id>` and (b) symlinks it created, and only after
//!   checking the symlink still points where it was recorded to point.
//! * Installing never overwrites a path Forge does not own.
//! * A failed install leaves the Forge root as it was.

mod scan;

use anyhow::{Context, Result, anyhow, bail};
use forge_core::util::{rm_rf, sh_quote, unix_now, validate_name, validate_version_id};
use forge_core::{DepRef, FileEntry, FileKind, Isolation, Manifest, Paths, RunSection};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

pub use scan::hash_file;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS packages(
    name         TEXT PRIMARY KEY,
    version      TEXT NOT NULL,
    version_id   TEXT NOT NULL,
    repo         TEXT NOT NULL,
    commit_sha   TEXT NOT NULL,
    build_system TEXT NOT NULL,
    installed_at INTEGER NOT NULL,
    manifest     TEXT NOT NULL,
    explicit     INTEGER NOT NULL DEFAULT 1
);
-- Files Forge placed outside its own root (e.g. display-manager session files).
-- Deliberately no foreign key: they must survive an update replacing the
-- package row, and `remove` refuses while any exist.
CREATE TABLE IF NOT EXISTS integrations(
    path   TEXT PRIMARY KEY,
    pkg    TEXT NOT NULL,
    sha256 TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS deps(
    pkg            TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
    dep            TEXT NOT NULL,
    dep_version_id TEXT NOT NULL,
    build_only     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(pkg, dep)
);
CREATE TABLE IF NOT EXISTS exports(
    link_path TEXT PRIMARY KEY,
    pkg       TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
    target    TEXT NOT NULL
);
";

/// Metadata supplied by the caller for a package that has just been built.
#[derive(Debug, Clone)]
pub struct NewPackage {
    pub name: String,
    pub version: String,
    pub version_id: String,
    pub repo: String,
    pub rev_spec: Option<String>,
    pub commit: String,
    pub build_system: String,
    /// Forge packages built against; each must be installed at exactly this version.
    pub deps: Vec<DepRef>,
    pub build_deps: Vec<String>,
    pub explicit: bool,
    /// How exported commands are launched. If this calls for a launcher,
    /// each exported executable gets a generated script in `.forge-run/`.
    pub run: RunSection,
    /// A directory already on the user's PATH (e.g. `~/.local/bin`) to also link
    /// each command into, so it works without editing any shell config. Links
    /// there are recorded and removed like all others; an existing file that
    /// Forge doesn't own is never replaced (the command stays available
    /// through `$FORGE_ROOT/bin`).
    pub link_dir: Option<PathBuf>,
    /// The data directory launchers read (e.g. `~/.local/share`): the package's menu entries
    /// (`share/applications/*.desktop`) and icons (`share/icons/**`) are linked into it, recorded
    /// and removed like the command links. `None`: not linked.
    pub share_dir: Option<PathBuf>,
    pub recipe: Option<String>,
}

#[derive(Debug)]
pub struct RemoveReport {
    pub name: String,
    pub version_id: String,
    pub package_dir: PathBuf,
    pub dir_existed: bool,
    pub file_count: usize,
    pub removed_links: Vec<PathBuf>,
    /// Links we did not delete because they are no longer ours.
    pub skipped_links: Vec<(PathBuf, String)>,
}

struct ExportPlan {
    /// Path inside the package of the real file.
    rel: String,
    link: PathBuf,
    target: String,
    base: String,
}

/// An open store. Holds an exclusive lock on `$FORGE_ROOT/lock` until dropped,
/// so keep it open only as long as needed (not across a build).
pub struct Store {
    paths: Paths,
    conn: Connection,
    _lock: fs::File,
}

impl Store {
    pub fn open(paths: &Paths) -> Result<Self> {
        paths.ensure_layout()?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(paths.lock_file())
            .context("opening lock file")?;
        lock.lock().context("acquiring the Forge lock")?;
        let conn = Connection::open(paths.db_file()).context("opening database")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        // Databases created before dependency support lack the `explicit` column.
        let has_explicit = conn
            .prepare("SELECT 1 FROM pragma_table_info('packages') WHERE name = 'explicit'")?
            .exists([])?;
        if !has_explicit {
            conn.execute_batch("ALTER TABLE packages ADD COLUMN explicit INTEGER NOT NULL DEFAULT 1;")?;
        }
        Ok(Store { paths: paths.clone(), conn, _lock: lock })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn get(&self, name: &str) -> Result<Option<Manifest>> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT manifest FROM packages WHERE name = ?1", [name], |r| r.get(0))
            .optional()?;
        json.map(|j| serde_json::from_str(&j).context("corrupt manifest in database"))
            .transpose()
    }

    pub fn list(&self) -> Result<Vec<Manifest>> {
        let mut st = self.conn.prepare("SELECT manifest FROM packages ORDER BY name")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for j in rows {
            out.push(serde_json::from_str(&j?).context("corrupt manifest in database")?);
        }
        Ok(out)
    }

    /// `(link path, recorded target)` for every link the package owns.
    pub fn exports(&self, name: &str) -> Result<Vec<(PathBuf, String)>> {
        exports_of(&self.conn, name)
    }

    /// Install a staged build as a package.
    ///
    /// `stage` is the DESTDIR the build installed into; the package lives at
    /// `stage/<final package dir>`. With `replace`, an existing install of the
    /// same name is swapped for the new one (update); the old one is deleted
    /// only after the new one is fully active.
    pub fn install(
        &mut self,
        np: NewPackage,
        stage: &Path,
        export_patterns: Option<&[String]>,
        replace: bool,
    ) -> Result<Manifest> {
        validate_name(&np.name)?;
        validate_version_id(&np.version_id)?;
        let pkgdir = self.paths.package_dir(&np.name, &np.version_id);
        let existing = self.get(&np.name)?;
        match (&existing, replace) {
            (Some(m), false) => bail!(
                "{} is already installed ({}); use `forge update {}`",
                np.name, m.version_id, np.name
            ),
            (Some(m), true) if m.version_id == np.version_id => {
                bail!("{} {} is already installed", np.name, np.version_id)
            }
            _ => {}
        }
        for d in &np.deps {
            match self.get(&d.name)? {
                Some(m) if m.version_id == d.version_id => {}
                Some(m) => bail!(
                    "dependency {} is at {} but {} was built against {}",
                    d.name, m.version_id, np.name, d.version_id
                ),
                None => bail!("dependency {} is not installed", d.name),
            }
            if d.name == np.name {
                bail!("{} cannot depend on itself", np.name);
            }
        }
        if fs::symlink_metadata(&pkgdir).is_ok() {
            bail!(
                "{} exists but is not recorded in the database (left over from a crashed install?). \
                 Inspect it and delete it manually.",
                pkgdir.display()
            );
        }

        let staged = stage.join(pkgdir.strip_prefix("/").context("package dir is not absolute")?);
        // Files installed to absolute paths (say /usr/lib/qt6/...) land outside the prefix. That is
        // the more useful thing to report, even when the prefix itself ended up empty.
        let outside = scan::staged_outside_prefix(stage, &staged)?;
        if !outside.is_empty() {
            let shown: Vec<_> = outside.iter().take(10).map(|s| format!("  /{s}")).collect();
            bail!(
                "the build tried to install files outside its prefix ({} total), refusing:\n{}",
                outside.len(),
                shown.join("\n")
            );
        }
        if !staged.is_dir() {
            bail!(
                "the build installed nothing under its prefix ({}). \
                 Does the project honour PREFIX and DESTDIR?",
                pkgdir.display()
            );
        }

        let mut files = scan::scan_tree(&staged, Some(&pkgdir))?;
        if files.iter().all(|f| f.kind == FileKind::Dir) {
            bail!("the build produced an empty package");
        }
        let mut plans = plan_exports(&self.paths, &np.name, &files, export_patterns)?;
        if np.run.needs_launcher() {
            if fs::symlink_metadata(staged.join(".forge-run")).is_ok() {
                bail!("package contains a reserved '.forge-run' entry");
            }
            let mut wrote = false;
            for p in plans.iter_mut() {
                let wrap = files.iter().find(|f| f.path == p.rel).is_some_and(|f| match f.kind {
                    FileKind::File => f.mode & 0o111 != 0,
                    FileKind::Symlink => p.rel.starts_with("bin/"),
                    FileKind::Dir => false,
                });
                if !wrap {
                    continue;
                }
                let script = launcher_script(&self.paths, &np.name, &p.rel, &p.base, &np.run)?;
                let dir = staged.join(".forge-run");
                fs::create_dir_all(&dir)?;
                let file = dir.join(&p.base);
                fs::write(&file, script)?;
                fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
                p.target = format!("../current/{}/.forge-run/{}", np.name, p.base);
                wrote = true;
            }
            if wrote {
                files = scan::scan_tree(&staged, Some(&pkgdir))?;
            }
        }
        self.preflight(&np.name, &plans, existing.is_some())?;
        let mut link_plans: Vec<ExportPlan> = Vec::new();
        if let Some(dir) = np.link_dir.as_deref().filter(|d| d.is_dir()) {
            for p in &plans {
                let link = dir.join(&p.base);
                if link == p.link {
                    continue; // the PATH directory is Forge's own bin
                }
                if fs::symlink_metadata(&link).is_ok() && self.export_owner(&link)?.as_deref() != Some(np.name.as_str()) {
                    eprintln!(
                        "warning: {} already exists and is not managed by Forge; left alone ({} is still available as {})",
                        link.display(),
                        p.base,
                        p.link.display()
                    );
                    continue;
                }
                link_plans.push(ExportPlan {
                    rel: p.rel.clone(),
                    link,
                    target: self.paths.bin().join(&p.base).to_string_lossy().into_owned(),
                    base: p.base.clone(),
                });
            }
        }

        let mut share_plans: Vec<ExportPlan> = Vec::new();
        if let Some(dir) = np.share_dir.as_deref() {
            for sp in share_links(&self.paths, &np.name, &files, dir) {
                if fs::symlink_metadata(&sp.link).is_ok() && self.export_owner(&sp.link)?.as_deref() != Some(np.name.as_str()) {
                    eprintln!("warning: {} already exists and is not managed by Forge; left alone", sp.link.display());
                    continue;
                }
                share_plans.push(sp);
            }
        }

        let manifest = Manifest {
            name: np.name.clone(),
            version: np.version,
            version_id: np.version_id.clone(),
            repo: np.repo,
            rev_spec: np.rev_spec,
            commit: np.commit,
            build_system: np.build_system,
            files,
            executables: plans.iter().map(|p| p.base.clone()).collect(),
            deps: np.deps,
            build_deps: np.build_deps,
            explicit: np.explicit,
            run: np.run,
            installed_at: unix_now(),
            forge_version: env!("CARGO_PKG_VERSION").to_string(),
            recipe: np.recipe,
        };

        let parent = pkgdir.parent().unwrap().to_path_buf();
        fs::create_dir_all(&parent)?;
        fs::rename(&staged, &pkgdir).with_context(|| {
            format!("moving build result into {} (same filesystem required)", pkgdir.display())
        })?;
        let all_plans: Vec<ExportPlan> = plans.into_iter().chain(link_plans).chain(share_plans).collect();
        if let Err(e) = self.activate(&manifest, &pkgdir, &all_plans, existing.as_ref()) {
            let _ = rm_rf(&pkgdir);
            let _ = fs::remove_dir(&parent); // only succeeds if empty
            return Err(e);
        }
        if let Some(old) = existing {
            let old_dir = self.paths.package_dir(&old.name, &old.version_id);
            if let Err(e) = self.discard_dir(&old.name, &old_dir) {
                eprintln!("warning: could not delete old version {}: {e:#}", old_dir.display());
            }
        }
        Ok(manifest)
    }

    /// Link an installed package's commands into `dir` (a directory on PATH),
    /// for installs made before linking existed or into a different directory.
    /// Returns the links created; existing ones and files Forge doesn't own
    /// are left alone.
    pub fn link_into(&mut self, name: &str, dir: &Path) -> Result<Vec<PathBuf>> {
        validate_name(name)?;
        let m = self.get(name)?.ok_or_else(|| anyhow!("package '{name}' is not installed"))?;
        if !dir.is_dir() {
            bail!("{} is not a directory", dir.display());
        }
        let mut created = Vec::new();
        for base in &m.executables {
            let link = dir.join(base);
            let root_link = self.paths.bin().join(base);
            if link == root_link || fs::symlink_metadata(&root_link).is_err() {
                continue;
            }
            if fs::symlink_metadata(&link).is_ok() {
                if self.export_owner(&link)?.as_deref() != Some(name) {
                    eprintln!("warning: {} already exists and is not managed by Forge; left alone", link.display());
                }
                continue;
            }
            let target = root_link.to_string_lossy().into_owned();
            self.conn.execute(
                "INSERT INTO exports(link_path, pkg, target) VALUES (?1, ?2, ?3)",
                params![link.to_string_lossy(), name, target],
            )?;
            if let Err(e) = symlink(&target, &link) {
                self.conn.execute("DELETE FROM exports WHERE link_path = ?1", [link.to_string_lossy()])?;
                return Err(e).with_context(|| format!("creating {}", link.display()));
            }
            created.push(link);
        }
        Ok(created)
    }

    /// Link an installed package's menu entries and icons into `dir` (see `NewPackage::share_dir`),
    /// for packages installed before that existed. Existing links and files Forge doesn't own are
    /// left alone. Returns the links created.
    pub fn link_share_into(&mut self, name: &str, dir: &Path) -> Result<Vec<PathBuf>> {
        validate_name(name)?;
        let m = self.get(name)?.ok_or_else(|| anyhow!("package '{name}' is not installed"))?;
        let mut created = Vec::new();
        for sp in share_links(&self.paths, name, &m.files, dir) {
            if fs::symlink_metadata(&sp.link).is_ok() {
                continue;
            }
            if let Some(parent) = sp.link.parent() {
                fs::create_dir_all(parent)?;
            }
            self.conn.execute("INSERT INTO exports(link_path, pkg, target) VALUES (?1, ?2, ?3)", params![sp.link.to_string_lossy(), name, sp.target])?;
            if let Err(e) = symlink(&sp.target, &sp.link) {
                self.conn.execute("DELETE FROM exports WHERE link_path = ?1", [sp.link.to_string_lossy()])?;
                return Err(e).with_context(|| format!("creating {}", sp.link.display()));
            }
            created.push(sp.link);
        }
        Ok(created)
    }

    /// The package that owns this link, if any.
    fn export_owner(&self, link: &Path) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT pkg FROM exports WHERE link_path = ?1", [link.to_string_lossy()], |r| r.get(0))
            .optional()?)
    }

    /// Refuse to proceed if any path we are about to create is already taken
    /// by something we do not own.
    fn preflight(&self, name: &str, plans: &[ExportPlan], replacing: bool) -> Result<()> {
        for p in plans {
            if fs::symlink_metadata(&p.link).is_ok() {
                let owner: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT pkg FROM exports WHERE link_path = ?1",
                        [p.link.to_string_lossy()],
                        |r| r.get(0),
                    )
                    .optional()?;
                match owner {
                    Some(o) if o == name && replacing => {}
                    Some(o) => bail!("{} is already provided by package '{o}'", p.link.display()),
                    None => bail!(
                        "{} already exists and is not managed by Forge; refusing to overwrite it",
                        p.link.display()
                    ),
                }
            }
        }
        let cur = self.paths.current().join(name);
        if fs::symlink_metadata(&cur).is_ok() && !replacing {
            bail!("{} already exists and is not managed by Forge", cur.display());
        }
        Ok(())
    }

    /// Write the manifest, update the database and create links, all or nothing.
    fn activate(
        &mut self,
        m: &Manifest,
        pkgdir: &Path,
        plans: &[ExportPlan],
        old: Option<&Manifest>,
    ) -> Result<()> {
        let meta_dir = pkgdir.join(".forge");
        fs::create_dir_all(&meta_dir)?;
        fs::write(meta_dir.join("manifest.json"), serde_json::to_vec_pretty(m)?)?;

        let paths = &self.paths;
        let old_exports = match old {
            Some(o) => exports_of(&self.conn, &o.name)?,
            None => Vec::new(),
        };
        let tx = self.conn.transaction()?;
        if old.is_some() {
            tx.execute("DELETE FROM packages WHERE name = ?1", [&m.name])?;
        }
        tx.execute(
            "INSERT INTO packages(name, version, version_id, repo, commit_sha, build_system, installed_at, manifest, explicit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                m.name, m.version, m.version_id, m.repo, m.commit, m.build_system,
                m.installed_at, serde_json::to_string(m)?, m.explicit
            ],
        )?;
        for d in &m.deps {
            tx.execute(
                "INSERT INTO deps(pkg, dep, dep_version_id, build_only) VALUES (?1, ?2, ?3, ?4)",
                params![m.name, d.name, d.version_id, d.build_only],
            )?;
        }
        for p in plans {
            tx.execute(
                "INSERT INTO exports(link_path, pkg, target) VALUES (?1, ?2, ?3)",
                params![p.link.to_string_lossy(), m.name, p.target],
            )?;
        }

        let mut undo = Undo::default();
        let fs_result = (|| -> Result<()> {
            if let Some(o) = old {
                for (link, target) in &old_exports {
                    if remove_link_if(link, target)? {
                        undo.removed.push((link.clone(), target.clone()));
                    }
                }
                let cur = paths.current().join(&o.name);
                let expected = current_target(&o.name, &o.version_id);
                if remove_link_if(&cur, &expected)? {
                    undo.removed.push((cur, expected));
                }
            }
            let cur = paths.current().join(&m.name);
            symlink(current_target(&m.name, &m.version_id), &cur)
                .with_context(|| format!("creating {}", cur.display()))?;
            undo.created.push(cur);
            for p in plans {
                if let Some(parent) = p.link.parent() {
                    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
                }
                symlink(&p.target, &p.link)
                    .with_context(|| format!("creating {}", p.link.display()))?;
                undo.created.push(p.link.clone());
            }
            Ok(())
        })();
        match fs_result {
            Ok(()) => {
                if let Err(e) = tx.commit() {
                    undo.run();
                    return Err(e.into());
                }
                Ok(())
            }
            Err(e) => {
                undo.run();
                drop(tx); // rolls back
                Err(e)
            }
        }
    }

    /// `$FORGE_ROOT/data/<name>`: where a package's launcher scripts point its
    /// configuration, data, cache and state. Owned by the package, deleted with it.
    pub fn data_dir(&self, name: &str) -> PathBuf {
        self.paths.user_data_dir(name)
    }

    /// Does the package's data directory hold anything besides empty directories?
    pub fn data_has_files(&self, name: &str) -> Result<bool> {
        validate_name(name)?;
        if self.paths.is_system() {
            return Ok(false);
        }
        fn any_file(dir: &Path) -> Result<bool> {
            let Ok(rd) = fs::read_dir(dir) else { return Ok(false) };
            for e in rd {
                let e = e?;
                if !fs::symlink_metadata(e.path())?.is_dir() || any_file(&e.path())? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        any_file(&self.data_dir(name))
    }

    /// Delete the package's data directory. Returns whether it existed.
    pub fn remove_data(&self, name: &str) -> Result<bool> {
        validate_name(name)?;
        if self.paths.is_system() {
            return Ok(false); // users' data lives in their homes; never touched from here
        }
        let dir = self.data_dir(name);
        match fs::symlink_metadata(&dir) {
            Err(_) => return Ok(false),
            Ok(m) if !m.is_dir() => bail!("refusing to delete {}: not a real directory", dir.display()),
            Ok(_) => {}
        }
        rm_rf(&dir).with_context(|| format!("deleting {}", dir.display()))?;
        let _ = fs::remove_dir(self.paths.root().join("data")); // only if empty
        Ok(true)
    }

    /// Packages that directly depend on `name`.
    pub fn dependents(&self, name: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT pkg FROM deps WHERE dep = ?1 ORDER BY pkg")?;
        let rows = st.query_map([name], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Packages that depend on `name`, directly or indirectly, dependents-first
    /// would be the reverse of this order (nearest dependents come first).
    pub fn dependents_transitive(&self, name: &str) -> Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        let mut queue = vec![name.to_string()];
        while let Some(n) = queue.pop() {
            for d in self.dependents(&n)? {
                if seen.insert(d.clone()) {
                    order.push(d.clone());
                    queue.push(d);
                }
            }
        }
        Ok(order)
    }

    /// System files recorded for `pkg`: `(path, sha256 of what Forge wrote)`.
    pub fn integrations(&self, pkg: &str) -> Result<Vec<(PathBuf, String)>> {
        let mut st = self.conn.prepare("SELECT path, sha256 FROM integrations WHERE pkg = ?1 ORDER BY path")?;
        let rows = st.query_map([pkg], |r| Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Who owns this system path, if anyone.
    pub fn integration_owner(&self, path: &Path) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT pkg FROM integrations WHERE path = ?1", [path.to_string_lossy()], |r| r.get(0))
            .optional()?)
    }

    pub fn record_integration(&mut self, pkg: &str, path: &Path, sha256: &str) -> Result<()> {
        if self.get(pkg)?.is_none() {
            bail!("package '{pkg}' is not installed");
        }
        self.conn.execute(
            "INSERT INTO integrations(path, pkg, sha256) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET sha256 = excluded.sha256 WHERE pkg = excluded.pkg",
            params![path.to_string_lossy(), pkg, sha256],
        )?;
        if self.integration_owner(path)?.as_deref() != Some(pkg) {
            bail!("{} is recorded as belonging to another package", path.display());
        }
        Ok(())
    }

    pub fn forget_integration(&mut self, path: &Path) -> Result<()> {
        self.conn.execute("DELETE FROM integrations WHERE path = ?1", [path.to_string_lossy()])?;
        Ok(())
    }

    /// Mark a package as explicitly installed (or not).
    pub fn set_explicit(&mut self, name: &str, explicit: bool) -> Result<()> {
        let mut m = self.get(name)?.ok_or_else(|| anyhow!("package '{name}' is not installed"))?;
        m.explicit = explicit;
        self.conn.execute(
            "UPDATE packages SET explicit = ?2, manifest = ?3 WHERE name = ?1",
            params![name, explicit, serde_json::to_string(&m)?],
        )?;
        // Keep the copy inside the package in sync (best effort; informational only).
        let f = self.paths.package_dir(&m.name, &m.version_id).join(".forge/manifest.json");
        if f.is_file() {
            let _ = fs::write(f, serde_json::to_vec_pretty(&m)?);
        }
        Ok(())
    }

    /// Automatically-installed packages that nothing would need any more once
    /// `removing` is gone, in a safe removal order (dependents first).
    pub fn orphans_if_removed(&self, removing: &[String]) -> Result<Vec<String>> {
        let mut gone: BTreeSet<String> = removing.iter().cloned().collect();
        let mut out = Vec::new();
        loop {
            let mut progressed = false;
            for m in self.list()? {
                if m.explicit || gone.contains(&m.name) {
                    continue;
                }
                if self.dependents(&m.name)?.iter().all(|d| gone.contains(d)) {
                    gone.insert(m.name.clone());
                    out.push(m.name);
                    progressed = true;
                }
            }
            if !progressed {
                return Ok(out);
            }
        }
    }

    /// Remove a package: its links, its directory, its database rows.
    pub fn remove(&mut self, name: &str, dry_run: bool) -> Result<RemoveReport> {
        self.remove_ignoring(name, dry_run, &[])
    }

    /// Like `remove`, but `ignore` lists packages that are being removed too
    /// and so do not count as dependents (used when removing a whole set).
    pub fn remove_ignoring(&mut self, name: &str, dry_run: bool, ignore: &[String]) -> Result<RemoveReport> {
        validate_name(name)?;
        let m = self
            .get(name)?
            .ok_or_else(|| anyhow!("package '{name}' is not installed"))?;
        let needed_by: Vec<String> =
            self.dependents(name)?.into_iter().filter(|d| !ignore.contains(d)).collect();
        if !needed_by.is_empty() {
            bail!("{name} is required by: {}. Remove those first.", needed_by.join(", "));
        }
        if !dry_run {
            let files = self.integrations(name)?;
            if !files.is_empty() {
                let list: Vec<_> = files.iter().map(|(p, _)| p.display().to_string()).collect();
                bail!(
                    "{name} has files outside the Forge root ({}); run `forge unintegrate {name}` first",
                    list.join(", ")
                );
            }
        }
        let pkgdir = self.paths.package_dir(&m.name, &m.version_id);
        let mut report = RemoveReport {
            name: m.name.clone(),
            version_id: m.version_id.clone(),
            package_dir: pkgdir.clone(),
            dir_existed: fs::symlink_metadata(&pkgdir).is_ok(),
            file_count: m.files.iter().filter(|f| f.kind != FileKind::Dir).count(),
            removed_links: Vec::new(),
            skipped_links: Vec::new(),
        };

        let mut links = self.exports(name)?;
        links.push((self.paths.current().join(name), current_target(name, &m.version_id)));
        for (link, target) in links {
            match fs::symlink_metadata(&link) {
                Err(_) => {} // already gone
                Ok(meta) if meta.file_type().is_symlink() && fs::read_link(&link)?.to_str() == Some(&target) => {
                    if !dry_run {
                        fs::remove_file(&link)?;
                    }
                    report.removed_links.push(link);
                }
                Ok(_) => report
                    .skipped_links
                    .push((link, "no longer points where Forge put it; left alone".into())),
            }
        }

        if dry_run {
            return Ok(report);
        }
        if report.dir_existed {
            self.discard_dir(name, &pkgdir)?;
        }
        self.conn.execute("DELETE FROM packages WHERE name = ?1", [name])?;
        let _ = fs::remove_dir(self.paths.store().join(name)); // only if empty
        Ok(report)
    }

    /// Delete a package directory: check it is exactly where Forge put it,
    /// move it out of the store atomically, then delete it.
    fn discard_dir(&self, name: &str, dir: &Path) -> Result<()> {
        let expected_parent = self.paths.store().join(name);
        if dir.parent() != Some(expected_parent.as_path()) || dir.file_name().is_none() {
            bail!("refusing to delete {}: not a package directory", dir.display());
        }
        let meta = match fs::symlink_metadata(dir) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        if !meta.is_dir() {
            bail!("refusing to delete {}: not a real directory", dir.display());
        }
        let trash = self
            .paths
            .trash()
            .join(format!("{name}-{}-{}", unix_now(), std::process::id()));
        fs::rename(dir, &trash)
            .with_context(|| format!("moving {} to trash", dir.display()))?;
        rm_rf(&trash).with_context(|| format!("deleting {}", trash.display()))?;
        Ok(())
    }

    /// Check installed packages against their manifests. Returns
    /// `(package, problem)` pairs; empty means everything is intact.
    pub fn verify(&self, only: Option<&str>) -> Result<Vec<(String, String)>> {
        let pkgs = match only {
            Some(n) => vec![self.get(n)?.ok_or_else(|| anyhow!("package '{n}' is not installed"))?],
            None => self.list()?,
        };
        let mut problems = Vec::new();
        for m in pkgs {
            let mut bad = |s: String| problems.push((m.name.clone(), s));
            let dir = self.paths.package_dir(&m.name, &m.version_id);
            if !dir.is_dir() {
                bad(format!("package directory {} is missing", dir.display()));
                continue;
            }
            let actual: BTreeMap<String, FileEntry> = match scan::scan_tree(&dir, None) {
                Ok(v) => v.into_iter().map(|f| (f.path.clone(), f)).collect(),
                Err(e) => {
                    bad(format!("cannot scan package: {e:#}"));
                    continue;
                }
            };
            let expected: BTreeMap<&str, &FileEntry> =
                m.files.iter().map(|f| (f.path.as_str(), f)).collect();
            for (p, e) in &expected {
                match actual.get(*p) {
                    None => bad(format!("missing: {p}")),
                    Some(a) if a.kind != e.kind => bad(format!("type changed: {p}")),
                    Some(a) if a.sha256 != e.sha256 => bad(format!("modified: {p}")),
                    Some(a) if a.link_target != e.link_target => bad(format!("link changed: {p}")),
                    Some(a) if a.mode != e.mode => bad(format!("permissions changed: {p}")),
                    Some(_) => {}
                }
            }
            for p in actual.keys() {
                if !expected.contains_key(p.as_str()) {
                    bad(format!("unexpected file: {p}"));
                }
            }
            for (path, sha) in self.integrations(&m.name)? {
                match fs::symlink_metadata(&path) {
                    Err(_) => bad(format!("integrated file is missing: {}", path.display())),
                    Ok(meta) if !meta.is_file() => bad(format!("integrated path is not a file: {}", path.display())),
                    Ok(_) => match scan::hash_file(&path) {
                        Ok(h) if h == sha => {}
                        Ok(_) => bad(format!("integrated file was modified: {}", path.display())),
                        Err(e) => bad(format!("cannot read {}: {e:#}", path.display())),
                    },
                }
            }
            for d in &m.deps {
                match self.get(&d.name)? {
                    None => bad(format!("dependency {} is not installed", d.name)),
                    Some(cur) if cur.version_id != d.version_id => bad(format!(
                        "built against {} {} but {} is installed; rebuild with `forge update {}`",
                        d.name, d.version_id, cur.version_id, m.name
                    )),
                    Some(_) => {}
                }
            }
            for (link, target) in self.exports(&m.name)? {
                match fs::read_link(&link) {
                    Err(_) => bad(format!("missing link: {}", link.display())),
                    Ok(t) if t.to_str() != Some(&target) => {
                        bad(format!("link {} points to {}", link.display(), t.display()))
                    }
                    Ok(_) if fs::metadata(&link).is_err() => {
                        bad(format!("dangling link: {}", link.display()))
                    }
                    Ok(_) => {}
                }
            }
        }
        Ok(problems)
    }
}

fn exports_of(conn: &Connection, name: &str) -> Result<Vec<(PathBuf, String)>> {
    let mut st = conn.prepare("SELECT link_path, target FROM exports WHERE pkg = ?1 ORDER BY link_path")?;
    let rows = st.query_map([name], |r| Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// `current/<name>` is a relative link into the store.
fn current_target(name: &str, version_id: &str) -> String {
    format!("../store/{name}/{version_id}")
}

/// Delete `link` only if it is a symlink pointing at `expected`.
fn remove_link_if(link: &Path, expected: &str) -> Result<bool> {
    match fs::symlink_metadata(link) {
        Ok(m) if m.file_type().is_symlink() && fs::read_link(link)?.to_str() == Some(expected) => {
            fs::remove_file(link)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[derive(Default)]
struct Undo {
    created: Vec<PathBuf>,
    removed: Vec<(PathBuf, String)>,
}

impl Undo {
    fn run(&self) {
        for p in self.created.iter().rev() {
            let _ = fs::remove_file(p);
        }
        for (p, t) in &self.removed {
            let _ = symlink(t, p);
        }
    }
}

/// Decide which files get linked into `$FORGE_ROOT/bin`.
/// Menu entries and icons in a package's files, as links into `dir`.
fn share_links(paths: &Paths, name: &str, files: &[FileEntry], dir: &Path) -> Vec<ExportPlan> {
    let mut out = Vec::new();
    for f in files.iter().filter(|f| f.kind != FileKind::Dir) {
        let Some(rest) = f.path.strip_prefix("share/") else { continue };
        let wanted = match rest.strip_prefix("applications/") {
            Some(b) => !b.contains('/') && b.ends_with(".desktop"),
            None => rest.starts_with("icons/") && [".png", ".svg", ".xpm"].iter().any(|e| rest.ends_with(e)),
        };
        if !wanted || rest.split('/').any(|c| c.is_empty() || c == "..") {
            continue;
        }
        out.push(ExportPlan {
            rel: f.path.clone(),
            link: dir.join(rest),
            target: paths.current().join(name).join(&f.path).to_string_lossy().into_owned(),
            base: rest.rsplit('/').next().unwrap_or(rest).to_string(),
        });
    }
    out
}

fn plan_exports(
    paths: &Paths,
    name: &str,
    files: &[FileEntry],
    patterns: Option<&[String]>,
) -> Result<Vec<ExportPlan>> {
    let is_leaf = |f: &&FileEntry| f.kind != FileKind::Dir;
    let in_dir = |f: &FileEntry, dir: &str| {
        f.path
            .strip_prefix(dir)
            .and_then(|r| r.strip_prefix('/'))
            .is_some_and(|rest| !rest.contains('/'))
    };
    let mut rels: Vec<String> = Vec::new();
    match patterns {
        None => rels.extend(files.iter().filter(is_leaf).filter(|f| in_dir(f, "bin")).map(|f| f.path.clone())),
        Some(pats) => {
            for p in pats {
                if let Some(dir) = p.strip_suffix("/*") {
                    rels.extend(files.iter().filter(is_leaf).filter(|f| in_dir(f, dir)).map(|f| f.path.clone()));
                } else if files.iter().filter(is_leaf).any(|f| &f.path == p) {
                    rels.push(p.clone());
                } else {
                    bail!("install.exports: '{p}' does not exist in the built package");
                }
            }
        }
    }
    rels.sort();
    rels.dedup();
    let mut seen = BTreeMap::new();
    let mut out = Vec::new();
    for rel in rels {
        let base = Path::new(&rel)
            .file_name()
            .and_then(|b| b.to_str())
            .ok_or_else(|| anyhow!("bad export path {rel}"))?
            .to_string();
        if let Some(prev) = seen.insert(base.clone(), rel.clone()) {
            bail!("exports '{prev}' and '{rel}' would both be linked as '{base}'");
        }
        out.push(ExportPlan {
            rel: rel.clone(),
            link: paths.bin().join(&base),
            target: format!("../current/{name}/{rel}"),
            base,
        });
    }
    Ok(out)
}

/// The script `bin/<cmd>` points at: sets up the package's private
/// configuration/data directories, then runs the real program.
fn launcher_script(paths: &Paths, name: &str, rel: &str, command: &str, run: &RunSection) -> Result<String> {
    let system = paths.is_system();
    let data_s = paths.user_data_dir(name).to_string_lossy().into_owned();
    // {data} is the package's data directory; {dep:NAME} is where Forge package NAME lives
    // (through its stable `current` link, so it survives that package being updated).
    let sub = |v: &str| -> String {
        let mut out = if system { v.to_string() } else { v.replace("{data}", &data_s) };
        while let Some(i) = out.find("{dep:") {
            let Some(len) = out[i..].find('}') else { break };
            let dep = out[i + 5..i + len].to_string();
            let repl = if validate_name(&dep).is_ok() { paths.current().join(&dep).to_string_lossy().into_owned() } else { String::new() };
            out.replace_range(i..i + len + 1, &repl);
        }
        out
    };
    // Quote a value; in the system install `{data}` becomes the script's $FORGE_DATA.
    let quote = |v: &str| -> String {
        let v = sub(v);
        if system {
            v.split("{data}").map(sh_quote).collect::<Vec<_>>().join("\"$FORGE_DATA\"")
        } else {
            sh_quote(&v)
        }
    };
    let current = paths.current().join(name);
    let iso = run.isolation()?;

    let mut dirs: Vec<&str> = Vec::new();
    if iso != Isolation::None {
        dirs.extend(["config", "data", "cache", "state"]);
    }
    if iso == Isolation::Home {
        dirs.push("home");
    }
    dirs.extend(run.dirs.iter().map(String::as_str));

    let data_line = if system {
        format!("FORGE_DATA=\"${{XDG_DATA_HOME:-$HOME/.local/share}}/forge/data/{name}\"")
    } else {
        format!("FORGE_DATA={}", sh_quote(&data_s))
    };
    let mut s = format!("#!/bin/sh\n# Generated by Forge for package {name}. Do not edit.\n{data_line}\n");
    if !dirs.is_empty() {
        let list: Vec<String> = dirs.iter().map(|d| format!("\"$FORGE_DATA\"/{}", sh_quote(d))).collect();
        s += &format!("mkdir -p {} 2>/dev/null\n", list.join(" "));
    }
    for (dest, src) in &run.seed {
        let d = format!("\"$FORGE_DATA\"/{}", sh_quote(dest));
        let from = sh_quote(&current.join(src).to_string_lossy());
        s += &format!(
            "[ -e {d} ] || {{ mkdir -p \"$(dirname {d})\" && cp -R {from} {d}; }} 2>/dev/null || true\n"
        );
    }
    if iso != Isolation::None {
        s += "export XDG_CONFIG_HOME=\"$FORGE_DATA/config\" XDG_DATA_HOME=\"$FORGE_DATA/data\" \
              XDG_CACHE_HOME=\"$FORGE_DATA/cache\" XDG_STATE_HOME=\"$FORGE_DATA/state\"\n";
    }
    if iso == Isolation::Home {
        s += "export HOME=\"$FORGE_DATA/home\"\n";
    }
    for (k, v) in &run.env {
        s += &format!("export {k}={}\n", quote(v));
    }
    let wants_args = run.args_commands.is_empty() || run.args_commands.iter().any(|c| c == command);
    let args: String = if wants_args {
        run.args.iter().map(|a| format!(" {}", quote(a))).collect()
    } else {
        String::new()
    };
    s += &format!("exec {}{args} \"$@\"\n", sh_quote(&current.join(rel).to_string_lossy()));
    Ok(s)
}
