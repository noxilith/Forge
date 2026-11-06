//! Turn a void-packages checkout into Forge recipes.
//!
//! Only templates Forge can build faithfully are converted: one https source archive with a
//! SHA-256, and a build style Forge has a backend for. Everything else is skipped with a
//! reason that goes into the report. Hooks and options that cannot be carried over are listed
//! in a comment at the top of each recipe so nobody has to guess what was left out.

use crate::template::{self, Template, has_unknown, words};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Dependencies that Forge or the machine supplies, never as packages.
const SKIP_DEPS: &[&str] = &[
    "gcc", "gcc-ada", "glibc", "glibc-devel", "libgcc", "libgcc-devel", "libstdc++", "libstdc++-devel", "musl",
    "musl-devel", "base-chroot", "base-files", "base-system", "make", "cmake", "meson", "ninja", "go", "rust",
    "cargo", "python3", "pkg-config", "autoconf", "automake", "libtool", "git", "patch", "linux-headers", "bash",
    "coreutils", "sed", "gawk", "grep", "binutils", "pkgconf",
];

/// Host tools a recipe can require, by Void package name → command.
const TOOLS: &[(&str, &str)] = &[
    ("pkg-config", "pkg-config"),
    ("pkgconf", "pkg-config"),
    ("perl", "perl"),
    ("bison", "bison"),
    ("flex", "flex"),
    ("gperf", "gperf"),
    ("m4", "m4"),
    ("nasm", "nasm"),
    ("yasm", "yasm"),
];

/// Variables Forge does not read; if a template sets one, the recipe says so.
const IGNORED_VARS: &[&str] = &[
    "make_build_args", "make_install_args", "make_cmd", "make_build_target", "make_install_target", "go_ldflags",
    "go_build_tags", "go_package", "go_import_path", "cargo_build_args", "cargo_install_args", "build_helper",
    "CFLAGS", "CXXFLAGS", "LDFLAGS", "CPPFLAGS", "configure_script", "cmake_builddir", "meson_builddir",
    "make_dirs", "conf_files", "skip_extraction", "patch_args", "hardening", "lib32disabled",
];

#[derive(Debug, Default)]
pub struct Converted {
    pub name: String,
    pub version: String,
    pub archive: String,
    pub sha256: String,
    pub desc: String,
    pub homepage: String,
    pub license: String,
    pub system: String,
    pub args: Vec<String>,
    pub subdir: Option<String>,
    pub tools: Vec<String>,
    pub deps: Vec<String>,
    pub build_deps: Vec<String>,
    pub patch_files: Vec<PathBuf>,
    pub notes: Vec<String>,
    pub missing: Vec<String>,
    pub cycles: Vec<String>,
    raw_deps: Vec<String>,
    raw_host: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub commit: String,
    pub total: usize,
    pub imported: usize,
    pub skipped: BTreeMap<String, Vec<String>>,
    pub with_hooks: usize,
    pub with_patches: usize,
}

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn qlist(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|s| q(s)).collect::<Vec<_>>().join(", "))
}

impl Converted {
    pub fn to_toml(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("# Imported from Void Linux's template for '{}'. Review before relying on it.\n", self.name));
        for n in &self.notes {
            s.push_str(&format!("# {n}\n"));
        }
        if !self.missing.is_empty() {
            let mut m = self.missing.clone();
            let more = m.len().saturating_sub(12);
            m.truncate(12);
            s.push_str(&format!("# needs, but Forge has no recipe for yet: {}{}\n", m.join(", "), if more > 0 { format!(" (+{more})") } else { String::new() }));
        }
        if !self.cycles.is_empty() {
            s.push_str(&format!("# dependency cycle broken by leaving out: {}\n", self.cycles.join(", ")));
        }
        s.push_str("[package]\n");
        s.push_str(&format!("name = {}\nversion = {}\narchive = {}\nsha256 = {}\nstrip_components = 1\n", q(&self.name), q(&self.version), q(&self.archive), q(&self.sha256)));
        if !self.desc.is_empty() {
            s.push_str(&format!("description = {}\n", q(&self.desc)));
        }
        if !self.homepage.is_empty() {
            s.push_str(&format!("homepage = {}\n", q(&self.homepage)));
        }
        if !self.license.is_empty() {
            s.push_str(&format!("license = {}\n", q(&self.license)));
        }
        if !self.deps.is_empty() {
            s.push_str(&format!("deps = {}\n", qlist(&self.deps)));
        }
        if !self.patch_files.is_empty() {
            let names: Vec<String> = self.patch_files.iter().map(|p| format!("patches/{}", p.file_name().unwrap().to_string_lossy())).collect();
            s.push_str(&format!("patches = {}\n", qlist(&names)));
        }
        s.push_str(&format!("\n[build]\nsystem = {}\n", q(&self.system)));
        if !self.args.is_empty() {
            s.push_str(&format!("configure_args = {}\n", qlist(&self.args)));
        }
        if let Some(d) = &self.subdir {
            s.push_str(&format!("subdir = {}\n", q(d)));
        }
        if !self.tools.is_empty() {
            s.push_str(&format!("tools = {}\n", qlist(&self.tools)));
        }
        if !self.build_deps.is_empty() {
            s.push_str(&format!("deps = {}\n", qlist(&self.build_deps)));
        }
        s
    }
}

/// `name` or `name>=1.0_1` → `name`; `virtual?x` → None.
fn dep_name(d: &str) -> Option<String> {
    if d.contains('?') || d.is_empty() || template::has_unknown(d) {
        return None;
    }
    let n: String = d.chars().take_while(|c| !matches!(c, '<' | '>' | '=')).collect();
    (!n.is_empty()).then_some(n)
}

/// Values like `/usr/libexec`: they would point outside the package's directory.
fn absolute_system_path(arg: &str) -> bool {
    let v = arg.split_once('=').map(|(_, v)| v).unwrap_or(arg);
    ["/usr", "/etc", "/var", "/run", "/lib", "/opt", "/bin", "/sbin"].iter().any(|p| v == *p || v.starts_with(&format!("{p}/")))
}

fn convert_one(name: &str, t: &Template, dir: &Path) -> Result<Converted, String> {
    let get = |k: &str| t.get(k).filter(|v| !v.trim().is_empty());
    if get("broken").is_some() {
        return Err("marked broken".into());
    }
    if get("restricted").is_some() {
        return Err("restricted (cannot be redistributed)".into());
    }
    if let Some(a) = get("only_for_archs") {
        if !a.split_whitespace().any(|x| x == "x86_64") {
            return Err("not for x86_64".into());
        }
    }
    let style = get("build_style").ok_or("no build_style (custom template)")?;
    let system = match style {
        "cmake" => "cmake",
        "meson" => "meson",
        "cargo" => "cargo",
        "go" => "go",
        "gnu-configure" | "configure" => "autotools",
        "gnu-makefile" => "make",
        other => return Err(format!("build_style '{other}' has no Forge backend")),
    };
    if let Some(f) = ["do_fetch", "do_extract", "do_patch", "do_configure", "do_build", "do_install"].iter().find(|f| t.funcs.contains(**f)) {
        return Err(format!("custom {f}() function"));
    }
    if get("create_wrksrc").is_some() {
        return Err("create_wrksrc layout".into());
    }
    let dist = get("distfiles").ok_or("no distfiles")?;
    if has_unknown(dist) {
        return Err("distfiles depends on something only a shell can compute".into());
    }
    let dfs = words(dist);
    if dfs.len() != 1 {
        return Err("more than one distfile".into());
    }
    let url = dfs[0].split('>').next().unwrap().to_string();
    if !url.starts_with("https://") {
        return Err("source is not an https URL".into());
    }
    let sums = get("checksum").map(words).unwrap_or_default();
    if sums.len() != 1 || sums[0].len() != 64 || !sums[0].chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("no single SHA-256 checksum".into());
    }
    let version = get("version").ok_or("no version")?;
    if has_unknown(version) {
        return Err("version is computed".into());
    }
    let mut c = Converted {
        name: name.to_string(),
        version: version.to_string(),
        archive: url,
        sha256: sums[0].to_lowercase(),
        desc: get("short_desc").filter(|d| !has_unknown(d)).unwrap_or("").to_string(),
        homepage: get("homepage").filter(|d| !has_unknown(d)).unwrap_or("").to_string(),
        license: get("license").filter(|d| !has_unknown(d)).unwrap_or("").to_string(),
        system: system.to_string(),
        ..Default::default()
    };
    if let Some(b) = get("build_wrksrc").filter(|b| !has_unknown(b)) {
        c.subdir = Some(b.to_string());
    }
    let mut dropped = 0;
    for a in get("configure_args").map(words).unwrap_or_default() {
        if has_unknown(&a) || absolute_system_path(&a) {
            dropped += 1;
        } else {
            c.args.push(a);
        }
    }
    if dropped > 0 {
        c.notes.push(format!("{dropped} configure argument(s) left out (computed values or paths outside the package)"));
    }
    let hooks: Vec<&str> = t.funcs.iter().map(String::as_str).filter(|f| f.starts_with("pre_") || f.starts_with("post_") || *f == "do_check").filter(|f| *f != "do_check").collect();
    if !hooks.is_empty() {
        c.notes.push(format!("hooks not translated: {}", hooks.join(", ")));
    }
    let ign: Vec<&str> = IGNORED_VARS.iter().copied().filter(|v| t.vars.contains_key(*v)).collect();
    if !ign.is_empty() {
        c.notes.push(format!("settings not translated: {}", ign.join(", ")));
    }
    for k in ["depends", "makedepends"] {
        c.raw_deps.extend(get(k).map(words).unwrap_or_default());
    }
    c.raw_host = get("hostmakedepends").map(words).unwrap_or_default();
    // Patches, applied in name order like xbps-src does.
    if let Ok(rd) = fs::read_dir(dir.join("patches")) {
        let mut p: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file() && matches!(p.extension().and_then(|e| e.to_str()), Some("patch" | "diff"))).collect();
        p.sort();
        c.patch_files = p;
    }
    Ok(c)
}

/// Load `NAME_SITE` mirror variables from void-packages' environment setup.
fn mirrors(void: &Path) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let Ok(text) = fs::read_to_string(void.join("common/environment/setup/misc.sh")) else { return m };
    for (k, v) in template::parse(&text).vars {
        if k.ends_with("_SITE") {
            m.insert(k, v);
        }
    }
    m
}

/// Parse one template with the mirrors preloaded.
pub fn read_template(text: &str, mirrors: &BTreeMap<String, String>) -> Template {
    // Mirrors are plain assignments, so prepend them and let the parser do the work.
    let mut pre = String::new();
    for (k, v) in mirrors {
        pre.push_str(&format!("{k}='{v}'\n"));
    }
    template::parse(&format!("{pre}{text}"))
}

pub fn run(void: &Path, out: &Path) -> Result<Report> {
    let srcpkgs = void.join("srcpkgs");
    let mirrors = mirrors(void);
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    let mut report = Report { commit: commit_of(void), ..Default::default() };
    let mut pkgs: BTreeMap<String, Converted> = BTreeMap::new();

    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for e in fs::read_dir(&srcpkgs).with_context(|| format!("reading {}", srcpkgs.display()))?.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if let Ok(target) = fs::read_link(&path) {
            if let Some(t) = target.file_name() {
                aliases.insert(name, t.to_string_lossy().into_owned());
            }
            continue;
        }
        if path.join("template").is_file() {
            names.push((name, path));
        }
    }
    names.sort();
    report.total = names.len();

    for (name, dir) in &names {
        let text = match fs::read(dir.join("template")) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(_) => {
                report.skipped.entry("unreadable template".into()).or_default().push(name.clone());
                continue;
            }
        };
        let t = read_template(&text, &mirrors);
        let res = if forge_core::util::validate_name(name).is_err() { Err("name not allowed in Forge".to_string()) } else { convert_one(name, &t, dir) };
        match res {
            Ok(c) => {
                pkgs.insert(name.clone(), c);
            }
            Err(why) => report.skipped.entry(why).or_default().push(name.clone()),
        }
    }

    resolve_deps(&mut pkgs, &aliases);

    let _ = fs::remove_dir_all(out);
    fs::create_dir_all(out)?;
    for c in pkgs.values() {
        let body = c.to_toml();
        // Never write a recipe Forge itself would reject.
        if let Err(e) = forge_core::Recipe::parse(&body) {
            report.skipped.entry(format!("generated recipe rejected: {}", e.to_string().lines().next().unwrap_or(""))).or_default().push(c.name.clone());
            continue;
        }
        if c.patch_files.is_empty() {
            fs::write(out.join(format!("{}.toml", c.name)), body)?;
        } else {
            let d = out.join(&c.name);
            fs::create_dir_all(d.join("patches"))?;
            fs::write(d.join("recipe.toml"), body)?;
            for p in &c.patch_files {
                fs::copy(p, d.join("patches").join(p.file_name().unwrap()))?;
            }
            report.with_patches += 1;
        }
        report.imported += 1;
        if c.notes.iter().any(|n| n.starts_with("hooks")) {
            report.with_hooks += 1;
        }
    }
    Ok(report)
}

fn commit_of(void: &Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(void)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Turn each package's raw dependency words into deps on other imported packages.
pub fn resolve_deps(pkgs: &mut BTreeMap<String, Converted>, aliases: &BTreeMap<String, String>) {
    let known: BTreeSet<String> = pkgs.keys().cloned().collect();
    let resolve = |d: &str| -> Option<String> {
        let mut n = dep_name(d)?;
        for _ in 0..6 {
            match aliases.get(&n) {
                Some(t) => n = t.clone(),
                None => break,
            }
        }
        Some(n)
    };
    for c in pkgs.values_mut() {
        let (mut deps, mut build, mut missing) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        let mut tools = BTreeSet::new();
        for d in &c.raw_deps {
            let Some(r) = resolve(d) else { continue };
            if r == c.name || SKIP_DEPS.contains(&r.as_str()) {
                continue;
            }
            if known.contains(&r) {
                deps.insert(r);
            } else {
                missing.insert(r);
            }
        }
        for d in &c.raw_host {
            let Some(r) = resolve(d) else { continue };
            if let Some((_, cmd)) = TOOLS.iter().find(|(n, _)| *n == r) {
                tools.insert(cmd.to_string());
            } else if r == c.name || SKIP_DEPS.contains(&r.as_str()) {
                continue;
            } else if known.contains(&r) {
                if !deps.contains(&r) {
                    build.insert(r);
                }
            } else {
                missing.insert(r);
            }
        }
        c.deps = deps.into_iter().collect();
        c.build_deps = build.into_iter().collect();
        c.tools = tools.into_iter().collect();
        c.missing = missing.into_iter().collect();
    }
    break_cycles(pkgs);
}

fn break_cycles(pkgs: &mut BTreeMap<String, Converted>) {
    // 0 = new, 1 = on the current path, 2 = done. Iterative so deep chains cannot overflow.
    let mut state: BTreeMap<String, u8> = BTreeMap::new();
    let names: Vec<String> = pkgs.keys().cloned().collect();
    for start in names {
        if state.get(&start).copied().unwrap_or(0) != 0 {
            continue;
        }
        let mut stack: Vec<(String, usize)> = vec![(start.clone(), 0)];
        state.insert(start, 1);
        while let Some((node, idx)) = stack.pop() {
            let edges: Vec<String> = {
                let c = &pkgs[&node];
                c.deps.iter().chain(c.build_deps.iter()).cloned().collect()
            };
            if idx >= edges.len() {
                state.insert(node, 2);
                continue;
            }
            stack.push((node.clone(), idx + 1));
            let next = &edges[idx];
            match state.get(next).copied().unwrap_or(0) {
                1 => {
                    let c = pkgs.get_mut(&node).unwrap();
                    c.deps.retain(|d| d != next);
                    c.build_deps.retain(|d| d != next);
                    c.cycles.push(next.clone());
                    // The edge list shrank; step back so no edge is skipped.
                    if let Some(top) = stack.last_mut() {
                        top.1 -= 1;
                    }
                }
                0 => {
                    state.insert(next.clone(), 1);
                    stack.push((next.clone(), 0));
                }
                _ => {}
            }
        }
    }
}

impl Report {
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str("# Import report\n\n");
        s.push_str(&format!("Source: void-packages `{}`\n\n", self.commit));
        s.push_str(&format!(
            "| | |\n|---|---|\n| Templates read | {} |\n| Recipes written | {} |\n| Skipped | {} |\n| With patches | {} |\n| With hooks Forge cannot carry over | {} |\n\n",
            self.total, self.imported, self.total - self.imported, self.with_patches, self.with_hooks
        ));
        s.push_str("These recipes were generated, not built. A recipe whose header lists untranslated hooks or settings, or dependencies without a recipe, may not build as it is.\n\n## Why templates were skipped\n\n");
        let mut reasons: Vec<(&String, &Vec<String>)> = self.skipped.iter().collect();
        reasons.sort_by_key(|(r, v)| (std::cmp::Reverse(v.len()), (*r).clone()));
        for (r, v) in reasons {
            s.push_str(&format!("* {} \u{2014} {}\n", v.len(), r));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(text: &str) -> Result<Converted, String> {
        let t = read_template(text, &BTreeMap::new());
        convert_one(t.get("pkgname").unwrap_or("x"), &t, Path::new("/nonexistent"))
    }

    const SUM: &str = "8aa01bd3faff2be1d94f231d76489ce6d9f781f51f6e93eafe82359d6d8669af";

    fn tpl(extra: &str) -> String {
        format!("pkgname=foo\nversion=1.0\nbuild_style=cmake\ndistfiles=\"https://x.org/foo-${{version}}.tar.gz\"\nchecksum={SUM}\nshort_desc=\"A foo\"\nlicense=MIT\n{extra}")
    }

    #[test]
    fn converts_a_plain_cmake_template() {
        let c = conv(&tpl("configure_args=\"-DA=ON -DPREFIX=/usr\"\nmakedepends=\"zlib-devel\"\nhostmakedepends=\"pkg-config\"\n")).unwrap();
        assert_eq!(c.archive, "https://x.org/foo-1.0.tar.gz");
        assert_eq!(c.system, "cmake");
        assert_eq!(c.args, ["-DA=ON"]);
        assert!(c.notes.iter().any(|n| n.contains("1 configure")));
        let body = {
            let mut c = c;
            c.tools = vec!["pkg-config".into()];
            c.to_toml()
        };
        let r = forge_core::Recipe::parse(&body).unwrap();
        assert_eq!(r.package.sha256.as_deref(), Some(SUM));
    }

    #[test]
    fn skips_what_forge_cannot_build_faithfully() {
        assert!(conv(&tpl("do_build() {\n\ttrue\n}\n")).unwrap_err().contains("do_build"));
        assert!(conv(&tpl("broken=yes\n")).unwrap_err().contains("broken"));
        assert!(conv(&tpl("").replace("cmake", "python3-module")).unwrap_err().contains("python3-module"));
        assert!(conv(&tpl("").replace("https://x.org", "http://x.org")).unwrap_err().contains("https"));
        assert!(conv(&tpl("").replace(SUM, "abc")).unwrap_err().contains("SHA-256"));
        assert!(conv(&tpl("distfiles=\"https://a/1.tgz https://a/2.tgz\"\n")).unwrap_err().contains("more than one"));
    }

    #[test]
    fn mirror_variables_resolve() {
        let mut m = BTreeMap::new();
        m.insert("GNU_SITE".to_string(), "https://ftp.gnu.org/gnu".to_string());
        let t = read_template(&tpl("distfiles=\"${GNU_SITE}/foo/foo-${version}.tar.xz\"\n"), &m);
        assert_eq!(t.get("distfiles"), Some("https://ftp.gnu.org/gnu/foo/foo-1.0.tar.xz"));
    }

    fn pkg(name: &str, raw: &[&str]) -> Converted {
        Converted { name: name.into(), raw_deps: raw.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn dependencies_follow_aliases_and_drop_unknowns_and_cycles() {
        let mut p = BTreeMap::new();
        p.insert("app".to_string(), pkg("app", &["libz-devel>=1.0_1", "libz", "ghost-devel", "virtual?font:mono", "glibc-devel", "app"]));
        p.insert("libz".to_string(), pkg("libz", &["app"]));
        let aliases = BTreeMap::from([("libz-devel".to_string(), "libz".to_string())]);
        resolve_deps(&mut p, &aliases);
        assert_eq!(p["app"].deps, ["libz"]);
        assert_eq!(p["app"].missing, ["ghost-devel"]);
        // app -> libz -> app: the edge that closes the loop is cut.
        assert!(p["libz"].deps.is_empty() && p["libz"].cycles == ["app"]);
    }

    #[test]
    fn host_tools_become_tools_and_other_host_deps_build_deps() {
        let mut p = BTreeMap::new();
        let mut a = pkg("a", &[]);
        a.raw_host = vec!["pkg-config".into(), "scdoc".into(), "cmake".into()];
        p.insert("a".to_string(), a);
        p.insert("scdoc".to_string(), pkg("scdoc", &[]));
        resolve_deps(&mut p, &BTreeMap::new());
        assert_eq!(p["a"].tools, ["pkg-config"]);
        assert_eq!(p["a"].build_deps, ["scdoc"]);
    }

    #[test]
    fn run_writes_recipes_patches_and_a_report() {
        let d = tempfile::tempdir().unwrap();
        let void = d.path().join("void");
        let src = void.join("srcpkgs");
        fs::create_dir_all(src.join("foo/patches")).unwrap();
        fs::create_dir_all(src.join("bar")).unwrap();
        fs::write(src.join("foo/template"), tpl("makedepends=\"bar-devel\"\n")).unwrap();
        fs::write(src.join("foo/patches/fix.patch"), "--- a/x\n+++ b/x\n").unwrap();
        fs::write(src.join("bar/template"), tpl("").replace("pkgname=foo", "pkgname=bar")).unwrap();
        std::os::unix::fs::symlink("bar", src.join("bar-devel")).unwrap();
        let out = d.path().join("out");
        let rep = run(&void, &out).unwrap();
        assert_eq!((rep.total, rep.imported), (2, 2));
        assert!(out.join("bar.toml").is_file());
        assert!(out.join("foo/patches/fix.patch").is_file());
        let foo = forge_core::Recipe::from_file(&out.join("foo/recipe.toml")).unwrap();
        assert_eq!(foo.package.deps, ["bar"]);
        assert_eq!(foo.patch_files().unwrap().len(), 1);
        assert!(rep.to_markdown().contains("Recipes written | 2"));
    }
}
