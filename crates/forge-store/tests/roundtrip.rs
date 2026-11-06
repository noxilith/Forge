//! The promise Forge makes: install, then remove, leaves the root as it was;
//! and nothing Forge doesn't own is ever touched.

use forge_core::Paths;
use forge_store::{NewPackage, Store};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Env {
    _root_dir: TempDir,
    _stage_dir: TempDir,
    paths: Paths,
    stage: PathBuf,
}

fn env() -> Env {
    let root_dir = tempfile::tempdir().unwrap();
    let stage_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();
    let paths = Paths::new(root);
    paths.ensure_layout().unwrap();
    Env { stage: stage_dir.path().to_path_buf(), _root_dir: root_dir, _stage_dir: stage_dir, paths }
}

fn np(name: &str, id: &str) -> NewPackage {
    NewPackage {
        name: name.into(),
        version: id.into(),
        version_id: id.into(),
        repo: "https://example.com/x".into(),
        rev_spec: None,
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        build_system: "make".into(),
        deps: vec![],
        build_deps: vec![],
        explicit: true,
        run: forge_core::RunSection { isolate: Some("none".into()), ..Default::default() },
        link_dir: None,
        recipe: None,
    }
}

/// Lay out `files` as a build would have installed them into DESTDIR.
fn make_stage(e: &Env, name: &str, id: &str, files: &[(&str, &str, u32)]) -> PathBuf {
    let stage = tempfile::Builder::new().tempdir_in(&e.stage).unwrap().keep();
    let prefix = stage.join(e.paths.package_dir(name, id).strip_prefix("/").unwrap());
    for (rel, content, mode) in files {
        let p = prefix.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(*mode)).unwrap();
    }
    stage
}

fn prefix_in(e: &Env, stage: &Path, name: &str, id: &str) -> PathBuf {
    stage.join(e.paths.package_dir(name, id).strip_prefix("/").unwrap())
}

/// Everything under the root except Forge's own database and lock file.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
            if rel == "db" || rel == "lock" {
                continue;
            }
            let m = fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                out.insert(rel, "dir".into());
                walk(root, &p, out);
            } else if m.file_type().is_symlink() {
                out.insert(rel, format!("link {}", fs::read_link(&p).unwrap().display()));
            } else {
                out.insert(rel, format!("file {:o} {}", m.permissions().mode(), fs::read_to_string(&p).unwrap()));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

const HELLO: &[(&str, &str, u32)] = &[
    ("bin/hello", "#!/bin/sh\necho hi\n", 0o755),
    ("share/hello/data.txt", "data", 0o644),
    ("lib/libhello.so.1", "lib", 0o644),
];

#[test]
fn install_then_remove_leaves_nothing_behind() {
    let e = env();
    let before = snapshot(e.paths.root());
    let mut store = Store::open(&e.paths).unwrap();

    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    let m = store.install(np("hello", "1.0-abc"), &stage, None, false).unwrap();
    assert_eq!(m.executables, vec!["hello"]);
    assert_eq!(m.files.iter().filter(|f| f.sha256.is_some()).count(), 3);

    // Reachable through the managed symlink chain, and recorded.
    let link = e.paths.bin().join("hello");
    assert_eq!(fs::read_to_string(&link).unwrap(), "#!/bin/sh\necho hi\n");
    assert_eq!(store.get("hello").unwrap().unwrap().version_id, "1.0-abc");
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(store.verify(None).unwrap().is_empty());
    assert!(e.paths.package_dir("hello", "1.0-abc").join(".forge/manifest.json").is_file());

    let report = store.remove("hello", false).unwrap();
    assert_eq!(report.removed_links.len(), 2); // bin/hello + current/hello
    assert!(report.skipped_links.is_empty());
    assert!(store.get("hello").unwrap().is_none());
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn dry_run_changes_nothing() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    store.install(np("hello", "1.0-abc"), &stage, None, false).unwrap();
    let installed = snapshot(e.paths.root());
    let r = store.remove("hello", true).unwrap();
    assert_eq!(r.removed_links.len(), 2);
    assert_eq!(snapshot(e.paths.root()), installed);
    assert!(store.get("hello").unwrap().is_some());
}

#[test]
fn refuses_to_overwrite_foreign_files() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    fs::write(e.paths.bin().join("hello"), "mine").unwrap();
    let before = snapshot(e.paths.root());
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    let err = store.install(np("hello", "1.0-abc"), &stage, None, false).unwrap_err();
    assert!(format!("{err:#}").contains("not managed by Forge"), "{err:#}");
    assert_eq!(snapshot(e.paths.root()), before);
    assert_eq!(fs::read_to_string(e.paths.bin().join("hello")).unwrap(), "mine");
}

#[test]
fn two_packages_cannot_claim_the_same_command() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let s1 = make_stage(&e, "a", "1-x", &[("bin/tool", "a", 0o755)]);
    store.install(np("a", "1-x"), &s1, None, false).unwrap();
    let before = snapshot(e.paths.root());
    let s2 = make_stage(&e, "b", "1-x", &[("bin/tool", "b", 0o755)]);
    let err = store.install(np("b", "1-x"), &s2, None, false).unwrap_err();
    assert!(format!("{err:#}").contains("already provided by package 'a'"), "{err:#}");
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn rejects_files_staged_outside_the_prefix() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let before = snapshot(e.paths.root());
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    fs::create_dir_all(stage.join("etc")).unwrap();
    fs::write(stage.join("etc/hello.conf"), "x").unwrap();
    let err = store.install(np("hello", "1.0-abc"), &stage, None, false).unwrap_err();
    assert!(format!("{err:#}").contains("/etc/hello.conf"), "{err:#}");
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn rejects_setuid_and_escaping_symlinks() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let before = snapshot(e.paths.root());

    let stage = make_stage(&e, "p", "1-x", &[("bin/su", "x", 0o4755)]);
    assert!(store.install(np("p", "1-x"), &stage, None, false).is_err());

    for (rel, target) in [("lib/a", "/etc/passwd"), ("lib/b", "../../../../outside"), ("lib/c", "/usr/bin/../../etc")] {
        let stage = make_stage(&e, "p", "1-x", &[("bin/ok", "x", 0o755)]);
        let pre = prefix_in(&e, &stage, "p", "1-x");
        fs::create_dir_all(pre.join("lib")).unwrap();
        symlink(target, pre.join(rel)).unwrap();
        let err = store.install(np("p", "1-x"), &stage, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{rel}: {err:#}");
    }
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn accepts_internal_symlinks() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "p", "1-x", &[("lib/libp.so.1", "x", 0o644), ("bin/p", "x", 0o755)]);
    let pre = prefix_in(&e, &stage, "p", "1-x");
    symlink("libp.so.1", pre.join("lib/libp.so")).unwrap();
    symlink(e.paths.package_dir("p", "1-x").join("lib/libp.so.1"), pre.join("lib/abs.so")).unwrap();
    symlink("../lib/libp.so.1", pre.join("bin/alias")).unwrap();
    store.install(np("p", "1-x"), &stage, None, false).unwrap();
    assert!(store.verify(None).unwrap().is_empty());
    store.remove("p", false).unwrap();
}

#[test]
fn remove_leaves_foreign_replacement_links_alone() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    store.install(np("hello", "1.0-abc"), &stage, None, false).unwrap();

    let link = e.paths.bin().join("hello");
    fs::remove_file(&link).unwrap();
    symlink("/somewhere/else", &link).unwrap();

    let report = store.remove("hello", false).unwrap();
    assert_eq!(report.skipped_links.len(), 1);
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("/somewhere/else"));
    assert!(!e.paths.package_dir("hello", "1.0-abc").exists());
}

#[test]
fn verify_detects_tampering() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    store.install(np("hello", "1.0-abc"), &stage, None, false).unwrap();
    let dir = e.paths.package_dir("hello", "1.0-abc");

    fs::write(dir.join("share/hello/data.txt"), "tampered").unwrap();
    fs::remove_file(dir.join("lib/libhello.so.1")).unwrap();
    fs::write(dir.join("bin/extra"), "x").unwrap();
    let problems: Vec<String> = store.verify(Some("hello")).unwrap().into_iter().map(|p| p.1).collect();
    assert!(problems.iter().any(|p| p == "modified: share/hello/data.txt"), "{problems:?}");
    assert!(problems.iter().any(|p| p == "missing: lib/libhello.so.1"), "{problems:?}");
    assert!(problems.iter().any(|p| p == "unexpected file: bin/extra"), "{problems:?}");
}

#[test]
fn update_swaps_versions_and_removes_the_old_one() {
    let e = env();
    let before = snapshot(e.paths.root());
    let mut store = Store::open(&e.paths).unwrap();
    let s1 = make_stage(&e, "hello", "1.0-aaa", &[("bin/hello", "v1", 0o755), ("bin/old-only", "x", 0o755)]);
    store.install(np("hello", "1.0-aaa"), &s1, None, false).unwrap();

    // Same name without `replace` is an error; same version with it too.
    let s2 = make_stage(&e, "hello", "2.0-bbb", &[("bin/hello", "v2", 0o755)]);
    assert!(store.install(np("hello", "2.0-bbb"), &s2, None, false).is_err());

    let m = store.install(np("hello", "2.0-bbb"), &s2, None, true).unwrap();
    assert_eq!(m.version_id, "2.0-bbb");
    assert_eq!(fs::read_to_string(e.paths.bin().join("hello")).unwrap(), "v2");
    assert!(fs::symlink_metadata(e.paths.bin().join("old-only")).is_err(), "stale link kept");
    assert!(!e.paths.package_dir("hello", "1.0-aaa").exists());
    assert!(store.verify(None).unwrap().is_empty());

    store.remove("hello", false).unwrap();
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn custom_exports_and_collisions_within_a_package() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let files = [("bin/a", "a", 0o755), ("bin/b", "b", 0o755), ("libexec/helper", "h", 0o755)];
    let stage = make_stage(&e, "p", "1-x", &files);
    let pats = vec!["bin/a".to_string(), "libexec/*".to_string()];
    let m = store.install(np("p", "1-x"), &stage, Some(&pats), false).unwrap();
    assert_eq!(m.executables, vec!["a", "helper"]);
    assert!(fs::symlink_metadata(e.paths.bin().join("b")).is_err());

    store.remove("p", false).unwrap();
    let stage = make_stage(&e, "p", "1-x", &files);
    let bad = vec!["bin/a".to_string(), "libexec/a".to_string()];
    let stage2 = make_stage(&e, "p", "1-x", &[("bin/a", "a", 0o755), ("libexec/a", "a", 0o755)]);
    assert!(store.install(np("p", "1-x"), &stage2, Some(&bad), false).is_err());
    let missing = vec!["bin/nope".to_string()];
    assert!(store.install(np("p", "1-x"), &stage, Some(&missing), false).is_err());
}

#[test]
fn invalid_names_and_missing_packages() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    assert!(store.remove("nope", false).is_err());
    assert!(store.remove("../evil", false).is_err());
    let stage = make_stage(&e, "x", "1-x", HELLO);
    assert!(store.install(np("../evil", "1-x"), &stage, None, false).is_err());
    assert!(store.install(np("x", "../1"), &stage, None, false).is_err());
}

#[test]
fn empty_build_is_rejected() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = tempfile::Builder::new().tempdir_in(&e.stage).unwrap().keep();
    let err = store.install(np("p", "1-x"), &stage, None, false).unwrap_err();
    assert!(format!("{err:#}").contains("installed nothing"), "{err:#}");
}

// ---- dependencies between Forge packages ----

fn with_dep(mut p: NewPackage, dep: &str, id: &str, explicit: bool) -> NewPackage {
    p.deps.push(forge_core::DepRef { name: dep.into(), version_id: id.into(), build_only: false });
    p.explicit = explicit;
    p
}

fn install_chain(e: &Env, store: &mut Store) {
    // app -> lib -> base ; lib and base are automatic dependencies.
    let s = make_stage(e, "base", "1-x", &[("lib/libbase.so", "b", 0o644)]);
    let mut base = np("base", "1-x");
    base.explicit = false;
    store.install(base, &s, None, false).unwrap();
    let s = make_stage(e, "lib", "1-x", &[("lib/liblib.so", "l", 0o644)]);
    store.install(with_dep(np("lib", "1-x"), "base", "1-x", false), &s, None, false).unwrap();
    let s = make_stage(e, "app", "1-x", &[("bin/app", "a", 0o755)]);
    store.install(with_dep(np("app", "1-x"), "lib", "1-x", true), &s, None, false).unwrap();
}

#[test]
fn dependencies_must_be_installed_at_the_recorded_version() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let before = snapshot(e.paths.root());
    let s = make_stage(&e, "lib", "1-x", &[("lib/liblib.so", "l", 0o644)]);
    let err = store.install(with_dep(np("lib", "1-x"), "base", "1-x", true), &s, None, false).unwrap_err();
    assert!(format!("{err:#}").contains("dependency base is not installed"), "{err:#}");
    assert_eq!(snapshot(e.paths.root()), before);

    let s = make_stage(&e, "base", "2-y", &[("lib/libbase.so", "b", 0o644)]);
    store.install(np("base", "2-y"), &s, None, false).unwrap();
    let s = make_stage(&e, "lib", "1-x", &[("lib/liblib.so", "l", 0o644)]);
    let err = store.install(with_dep(np("lib", "1-x"), "base", "1-x", true), &s, None, false).unwrap_err();
    assert!(format!("{err:#}").contains("built against 1-x"), "{err:#}");
}

#[test]
fn cannot_remove_a_package_that_is_still_needed() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    install_chain(&e, &mut store);
    let before = snapshot(e.paths.root());
    let err = store.remove("lib", false).unwrap_err();
    assert!(format!("{err:#}").contains("required by: app"), "{err:#}");
    assert!(store.remove("base", false).is_err());
    assert_eq!(snapshot(e.paths.root()), before);
    assert_eq!(store.dependents("base").unwrap(), vec!["lib"]);
    assert_eq!(store.dependents_transitive("base").unwrap(), vec!["lib", "app"]);
}

#[test]
fn removing_the_top_package_orphans_its_automatic_dependencies() {
    let e = env();
    let before = snapshot(e.paths.root());
    let mut store = Store::open(&e.paths).unwrap();
    install_chain(&e, &mut store);
    assert!(store.verify(None).unwrap().is_empty());

    // Dependents come first, and only automatic packages are candidates.
    assert_eq!(store.orphans_if_removed(&["app".into()]).unwrap(), vec!["lib", "base"]);
    assert!(store.orphans_if_removed(&[]).unwrap().is_empty());

    for name in ["app", "lib", "base"] {
        store.remove(name, false).unwrap();
    }
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn explicit_packages_are_never_orphans_and_promotion_keeps_a_dependency() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    install_chain(&e, &mut store);
    // The user asks for `lib` explicitly: it must survive removal of `app`.
    store.set_explicit("lib", true).unwrap();
    assert!(store.get("lib").unwrap().unwrap().explicit);
    assert_eq!(store.orphans_if_removed(&["app".into()]).unwrap(), Vec::<String>::new());
    store.remove("app", false).unwrap();
    assert!(store.get("lib").unwrap().is_some());
    assert!(store.get("base").unwrap().is_some());
}

#[test]
fn verify_flags_dependents_built_against_an_older_version() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let s = make_stage(&e, "base", "1-x", &[("lib/libbase.so", "b", 0o644)]);
    store.install(np("base", "1-x"), &s, None, false).unwrap();
    let s = make_stage(&e, "lib", "1-x", &[("lib/liblib.so", "l", 0o644)]);
    store.install(with_dep(np("lib", "1-x"), "base", "1-x", true), &s, None, false).unwrap();

    // Updating `base` underneath `lib` is detected.
    let s = make_stage(&e, "base", "2-y", &[("lib/libbase.so", "b2", 0o644)]);
    store.install(np("base", "2-y"), &s, None, true).unwrap();
    let problems: Vec<String> = store.verify(None).unwrap().into_iter().map(|p| p.1).collect();
    assert!(problems.iter().any(|p| p.contains("built against base 1-x but 2-y is installed")), "{problems:?}");

    // Rebuilding the dependent (new version id, same name) clears it.
    let s = make_stage(&e, "lib", "1-x-d2", &[("lib/liblib.so", "l", 0o644)]);
    store.install(with_dep(np("lib", "1-x-d2"), "base", "2-y", true), &s, None, true).unwrap();
    assert!(store.verify(None).unwrap().is_empty());
}

#[test]
fn self_dependency_is_rejected() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let s = make_stage(&e, "x", "1-x", HELLO);
    assert!(store.install(with_dep(np("x", "1-x"), "x", "1-x", true), &s, None, false).is_err());
}

// ---- integrations (files outside the Forge root) ----

#[test]
fn integrations_block_removal_and_survive_updates() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sys = outside.path().join("forge-x.desktop");
    fs::write(&sys, "[Desktop Entry]\n").unwrap();
    let sha = forge_store::hash_file(&sys).unwrap();

    let s = make_stage(&e, "x", "1-a", HELLO);
    store.install(np("x", "1-a"), &s, None, false).unwrap();
    assert!(store.record_integration("nope", &sys, &sha).is_err());
    store.record_integration("x", &sys, &sha).unwrap();
    assert_eq!(store.integration_owner(&sys).unwrap().as_deref(), Some("x"));
    assert!(store.verify(None).unwrap().is_empty());

    // Another package cannot claim the same system path.
    let s2 = make_stage(&e, "y", "1-a", &[("bin/y", "y", 0o755)]);
    store.install(np("y", "1-a"), &s2, None, false).unwrap();
    assert!(store.record_integration("y", &sys, &sha).is_err());
    assert_eq!(store.integration_owner(&sys).unwrap().as_deref(), Some("x"));

    // Removal is refused (dry run is allowed), and nothing is touched.
    let before = snapshot(e.paths.root());
    let err = store.remove("x", false).unwrap_err();
    assert!(format!("{err:#}").contains("forge unintegrate x"), "{err:#}");
    store.remove("x", true).unwrap();
    assert_eq!(snapshot(e.paths.root()), before);

    // An update replacing the package keeps its integration record.
    let s3 = make_stage(&e, "x", "2-b", HELLO);
    store.install(np("x", "2-b"), &s3, None, true).unwrap();
    assert_eq!(store.integrations("x").unwrap().len(), 1);

    // Tampering with the system file is noticed.
    fs::write(&sys, "changed").unwrap();
    let problems: Vec<String> = store.verify(Some("x")).unwrap().into_iter().map(|p| p.1).collect();
    assert!(problems.iter().any(|p| p.contains("integrated file was modified")), "{problems:?}");
    fs::remove_file(&sys).unwrap();
    let problems: Vec<String> = store.verify(Some("x")).unwrap().into_iter().map(|p| p.1).collect();
    assert!(problems.iter().any(|p| p.contains("integrated file is missing")), "{problems:?}");

    store.forget_integration(&sys).unwrap();
    store.remove("x", false).unwrap();
    store.remove("y", false).unwrap();
}

// ---- launcher scripts and the package's data directory ----

fn run_xdg() -> forge_core::RunSection {
    forge_core::RunSection::default() // isolate defaults to "xdg"
}

fn run_cmd(e: &Env, name: &str, args: &[&str], envs: &[(&str, &str)]) -> String {
    let mut c = std::process::Command::new(e.paths.bin().join(name));
    c.args(args).env_remove("XDG_CONFIG_HOME");
    for (k, v) in envs {
        c.env(k, v);
    }
    // Executing a script that another test thread still has open for writing fails with
    // ETXTBSY ("text file busy"); that is a race in the test, so retry it.
    for _ in 0..50 {
        match c.output() {
            Ok(o) => return String::from_utf8_lossy(&o.stdout).to_string(),
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(std::time::Duration::from_millis(40)),
            Err(e) => panic!("{e}"),
        }
    }
    panic!("script stayed busy");
}

#[test]
fn launchers_redirect_config_into_the_package_and_removal_deletes_it() {
    let e = env();
    let before = snapshot(e.paths.root());
    let mut store = Store::open(&e.paths).unwrap();
    let prog = "#!/bin/sh\necho \"cfg=$XDG_CONFIG_HOME data=$XDG_DATA_HOME cache=$XDG_CACHE_HOME state=$XDG_STATE_HOME home=$HOME args=$*\"\n";
    let stage = make_stage(&e, "app", "1-x", &[("bin/app", prog, 0o755), ("lib/x", "x", 0o644)]);
    let mut p = np("app", "1-x");
    p.run = run_xdg();
    let m = store.install(p, &stage, None, false).unwrap();

    // The command is a launcher, owned and hashed like any other file.
    assert!(m.files.iter().any(|f| f.path == ".forge-run/app" && f.sha256.is_some()));
    assert_eq!(fs::read_link(e.paths.bin().join("app")).unwrap(), Path::new("../current/app/.forge-run/app"));
    assert!(store.verify(None).unwrap().is_empty());

    let data = e.paths.root().join("data/app");
    assert!(!data.exists(), "nothing is created until the program runs");
    let out = run_cmd(&e, "app", &["a", "b"], &[("HOME", "/real/home")]);
    let d = data.display();
    assert_eq!(
        out.trim(),
        format!("cfg={d}/config data={d}/data cache={d}/cache state={d}/state home=/real/home args=a b")
    );
    assert!(data.join("config").is_dir());

    // Not "has files" while only empty directories exist; yes once the program wrote something.
    assert!(!store.data_has_files("app").unwrap());
    fs::write(data.join("config/app.conf"), "mine").unwrap();
    assert!(store.data_has_files("app").unwrap());

    store.remove("app", false).unwrap();
    assert!(data.exists(), "remove() leaves data to the caller's decision");
    assert!(store.remove_data("app").unwrap());
    assert!(!store.remove_data("app").unwrap());
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn launcher_supports_home_env_args_dirs_and_seeding() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let prog = "#!/bin/sh\necho \"home=$HOME cfgenv=$MY_CONFIG xdg=${XDG_CONFIG_HOME-unset} args=$*\"\ncat \"$MY_CONFIG\"\n";
    let stage = make_stage(&e, "app", "1-x", &[("bin/app", prog, 0o755), ("share/app/default.conf", "seeded-default", 0o644)]);
    let mut p = np("app", "1-x");
    p.run = forge_core::RunSection {
        isolate: Some("none".into()),
        env: [("MY_CONFIG".to_string(), "{data}/config/app/app.conf".to_string())].into(),
        args: vec!["--config".into(), "{data}/x y".into()],
        dirs: vec!["config/app".into()],
        seed: [("config/app/app.conf".to_string(), "share/app/default.conf".to_string())].into(),
        ..Default::default()
    };
    store.install(p, &stage, None, false).unwrap();
    let data = e.paths.root().join("data/app");
    let out = run_cmd(&e, "app", &["z"], &[("HOME", "/real")]);
    // isolate = none leaves XDG and HOME alone; only the targeted variable and args are set.
    assert!(out.contains("home=/real "), "{out}");
    assert!(out.contains("xdg=unset"), "{out}");
    assert!(out.contains(&format!("cfgenv={}/config/app/app.conf", data.display())), "{out}");
    assert!(out.contains(&format!("args=--config {}/x y z", data.display())), "{out}");
    assert!(out.ends_with("seeded-default"), "{out}");

    // Seeding never overwrites what the user edited.
    fs::write(data.join("config/app/app.conf"), "edited").unwrap();
    assert!(run_cmd(&e, "app", &[], &[]).ends_with("edited"));

    // isolate = "home" sets HOME too.
    store.remove("app", false).unwrap();
    store.remove_data("app").unwrap();
    let plain = "#!/bin/sh\necho \"home=$HOME\"\n";
    let stage = make_stage(&e, "app", "1-x", &[("bin/app", plain, 0o755)]);
    let mut p = np("app", "1-x");
    p.run = forge_core::RunSection { isolate: Some("home".into()), ..Default::default() };
    store.install(p, &stage, None, false).unwrap();
    let out = run_cmd(&e, "app", &[], &[("HOME", "/real")]);
    assert_eq!(out.trim(), format!("home={}/home", data.display()));
}

#[test]
fn launchers_only_wrap_executables_and_updates_keep_data() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "app", "1-x", &[("bin/tool", "#!/bin/sh\necho v1\n", 0o755), ("bin/notes.txt", "not executable", 0o644)]);
    let mut p = np("app", "1-x");
    p.run = run_xdg();
    store.install(p, &stage, None, false).unwrap();
    assert_eq!(fs::read_link(e.paths.bin().join("tool")).unwrap(), Path::new("../current/app/.forge-run/tool"));
    assert_eq!(fs::read_link(e.paths.bin().join("notes.txt")).unwrap(), Path::new("../current/app/bin/notes.txt"));

    run_cmd(&e, "tool", &[], &[]);
    fs::write(e.paths.root().join("data/app/config/keep.conf"), "user data").unwrap();
    let stage = make_stage(&e, "app", "2-y", &[("bin/tool", "#!/bin/sh\necho v2\n", 0o755)]);
    let mut p = np("app", "2-y");
    p.run = run_xdg();
    store.install(p, &stage, None, true).unwrap();
    assert_eq!(run_cmd(&e, "tool", &[], &[]).trim(), "v2");
    assert_eq!(fs::read_to_string(e.paths.root().join("data/app/config/keep.conf")).unwrap(), "user data");
    assert!(store.verify(None).unwrap().is_empty());
}

#[test]
fn a_package_cannot_ship_its_own_launcher_directory() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let before = snapshot(e.paths.root());
    let stage = make_stage(&e, "app", "1-x", &[("bin/tool", "x", 0o755), (".forge-run/tool", "evil", 0o755)]);
    let mut p = np("app", "1-x");
    p.run = run_xdg();
    assert!(format!("{:#}", store.install(p, &stage, None, false).unwrap_err()).contains(".forge-run"));
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn launcher_args_can_be_limited_to_some_commands() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let prog = "#!/bin/sh\necho \"args=$*\"\n";
    let stage = make_stage(&e, "wm", "1-x", &[("bin/wm", prog, 0o755), ("bin/wmctl", prog, 0o755)]);
    let mut p = np("wm", "1-x");
    p.run = forge_core::RunSection {
        isolate: Some("none".into()),
        args: vec!["--config".into(), "{data}/c.conf".into()],
        args_commands: vec!["wm".into()],
        ..Default::default()
    };
    store.install(p, &stage, None, false).unwrap();
    let data = e.paths.root().join("data/wm");
    assert_eq!(run_cmd(&e, "wm", &["x"], &[]).trim(), format!("args=--config {}/c.conf x", data.display()));
    assert_eq!(run_cmd(&e, "wmctl", &["x"], &[]).trim(), "args=x");
}


// ---- linking commands into a directory that is already on PATH ----

#[test]
fn commands_are_also_linked_into_the_path_directory_and_removed_with_the_package() {
    let e = env();
    let before = snapshot(e.paths.root());
    let linkdir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    let mut p = np("hello", "1.0-abc");
    p.link_dir = Some(linkdir.path().to_path_buf());
    let m = store.install(p, &stage, None, false).unwrap();
    assert_eq!(m.executables, vec!["hello"], "the PATH link is not a second command");

    let link = linkdir.path().join("hello");
    assert_eq!(fs::read_link(&link).unwrap(), e.paths.bin().join("hello"));
    assert_eq!(fs::read_to_string(&link).unwrap(), "#!/bin/sh\necho hi\n");
    assert!(store.verify(None).unwrap().is_empty());
    assert_eq!(store.exports("hello").unwrap().len(), 2);

    // An update replaces it cleanly; removal deletes it.
    let stage = make_stage(&e, "hello", "2.0-def", &[("bin/hello", "v2", 0o755)]);
    let mut p = np("hello", "2.0-def");
    p.link_dir = Some(linkdir.path().to_path_buf());
    store.install(p, &stage, None, true).unwrap();
    assert_eq!(fs::read_to_string(&link).unwrap(), "v2");
    let report = store.remove("hello", false).unwrap();
    assert_eq!(report.removed_links.len(), 3); // root bin, PATH dir, current
    assert!(fs::symlink_metadata(&link).is_err());
    assert_eq!(snapshot(e.paths.root()), before);
}

#[test]
fn an_existing_file_in_the_path_directory_is_never_replaced() {
    let e = env();
    let linkdir = tempfile::tempdir().unwrap();
    fs::write(linkdir.path().join("hello"), "the user's own program").unwrap();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    let mut p = np("hello", "1.0-abc");
    p.link_dir = Some(linkdir.path().to_path_buf());
    store.install(p, &stage, None, false).unwrap(); // still installs
    assert_eq!(fs::read_to_string(linkdir.path().join("hello")).unwrap(), "the user's own program");
    assert!(fs::symlink_metadata(e.paths.bin().join("hello")).is_ok(), "still reachable through Forge's bin");
    store.remove("hello", false).unwrap();
    assert_eq!(fs::read_to_string(linkdir.path().join("hello")).unwrap(), "the user's own program");
}

#[test]
fn a_missing_path_directory_is_ignored() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    let mut p = np("hello", "1.0-abc");
    p.link_dir = Some(PathBuf::from("/nonexistent/forge-test-dir"));
    store.install(p, &stage, None, false).unwrap();
    assert_eq!(store.exports("hello").unwrap().len(), 1);
}

#[test]
fn link_into_repairs_installs_made_without_a_path_link() {
    let e = env();
    let linkdir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = make_stage(&e, "hello", "1.0-abc", HELLO);
    store.install(np("hello", "1.0-abc"), &stage, None, false).unwrap(); // no link_dir
    assert!(store.link_into("nope", linkdir.path()).is_err());

    let made = store.link_into("hello", linkdir.path()).unwrap();
    assert_eq!(made, vec![linkdir.path().join("hello")]);
    assert_eq!(fs::read_to_string(linkdir.path().join("hello")).unwrap(), "#!/bin/sh\necho hi\n");
    assert!(store.verify(None).unwrap().is_empty());
    assert!(store.link_into("hello", linkdir.path()).unwrap().is_empty(), "idempotent");

    // Recorded, so removal cleans it up.
    store.remove("hello", false).unwrap();
    assert!(fs::symlink_metadata(linkdir.path().join("hello")).is_err());
}

#[test]
fn launcher_values_can_point_at_a_dependency() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let prog = "#!/bin/sh\necho \"qml=$QML_IMPORT_PATH\"\n";
    let stage = make_stage(&e, "app", "1-x", &[("bin/app", prog, 0o755)]);
    let mut p = np("app", "1-x");
    p.run = forge_core::RunSection {
        isolate: Some("none".into()),
        env: [("QML_IMPORT_PATH".to_string(), "{dep:qt-support}/lib/qt6/qml:{data}/qml".to_string())].into(),
        ..Default::default()
    };
    store.install(p, &stage, None, false).unwrap();
    let out = run_cmd(&e, "app", &[], &[]);
    let root = e.paths.root().display();
    assert_eq!(out.trim(), format!("qml={root}/current/qt-support/lib/qt6/qml:{root}/data/app/qml"));
}

#[test]
fn files_installed_outside_the_prefix_are_reported_even_when_the_prefix_is_empty() {
    let e = env();
    let mut store = Store::open(&e.paths).unwrap();
    let stage = tempfile::Builder::new().tempdir_in(&e.stage).unwrap().keep();
    fs::create_dir_all(stage.join("usr/lib/qt6/qml/x")).unwrap();
    fs::write(stage.join("usr/lib/qt6/qml/x/qmldir"), "module x").unwrap();
    let err = format!("{:#}", store.install(np("p", "1-x"), &stage, None, false).unwrap_err());
    assert!(err.contains("outside its prefix") && err.contains("/usr/lib/qt6/qml/x/qmldir"), "{err}");
    assert!(!err.contains("installed nothing"), "{err}");
}

#[test]
fn system_install_keeps_each_users_data_in_their_own_home() {
    let root_dir = tempfile::tempdir().unwrap();
    let stage_dir = tempfile::tempdir().unwrap();
    let paths = Paths::new_system(root_dir.path().canonicalize().unwrap());
    paths.ensure_layout().unwrap();
    let e = Env { stage: stage_dir.path().to_path_buf(), _root_dir: root_dir, _stage_dir: stage_dir, paths };
    let mut store = Store::open(&e.paths).unwrap();
    let prog = "#!/bin/sh\necho \"xdg=$XDG_CONFIG_HOME arg=$*\"\n";
    let stage = make_stage(&e, "app", "1-x", &[("bin/app", prog, 0o755)]);
    let mut p = np("app", "1-x");
    p.run = forge_core::RunSection { args: vec!["--dir".into(), "{data}/my dir".into()], ..Default::default() };
    store.install(p, &stage, None, false).unwrap();

    // The script holds no user's path: two users get two data directories.
    let script = fs::read_to_string(e.paths.bin().join("app")).unwrap();
    assert!(script.contains("$FORGE_DATA") && !script.contains(&e.paths.root().to_string_lossy().to_string().replace("/", "//")), "{script}");
    for user in ["alice", "bob"] {
        let home = tempfile::tempdir().unwrap();
        let out = run_cmd(&e, "app", &[], &[("XDG_DATA_HOME", &home.path().to_string_lossy()), ("HOME", user)]);
        let data = home.path().join("forge/data/app");
        assert!(out.contains(&format!("xdg={}/config", data.display())), "{out}");
        assert!(out.contains(&format!("arg=--dir {}/my dir", data.display())), "{out}");
        assert!(data.join("config").is_dir());
    }
    // Nothing of the users' data lives in (or is removed from) the shared tree.
    assert!(!e.paths.root().join("data").join("app").exists());
    store.remove("app", false).unwrap();
    assert!(!store.remove_data("app").unwrap());
}
