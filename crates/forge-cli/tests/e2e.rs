//! Full pipeline against a local git repository: clone, pin, sandboxed build,
//! install, run, verify, update, remove.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MAKEFILE: &str = "PREFIX ?= /usr/local
all: hello
hello: hello.sh
\tcp hello.sh hello && chmod +x hello
install: all
\tinstall -d $(DESTDIR)$(PREFIX)/bin $(DESTDIR)$(PREFIX)/share/hello
\tinstall -m755 hello $(DESTDIR)$(PREFIX)/bin/hello
\tinstall -m644 hello.sh $(DESTDIR)$(PREFIX)/share/hello/source.sh
";

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    sandbox: bool,
}

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn commit_all(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", msg]);
}

fn fixture(makefile: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("hello");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("Makefile"), makefile).unwrap();
    fs::write(repo.join("hello.sh"), "#!/bin/sh\necho hello-v1\n").unwrap();
    commit_all(&repo, "v1");
    git(&repo, &["tag", "v1.0.0"]);
    let sandbox = Command::new("bwrap")
        .args(["--unshare-all", "--ro-bind", "/", "/", "--dev", "/dev", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    fs::create_dir_all(base.join("wayland-sessions")).unwrap();
    fs::create_dir_all(base.join("xsessions")).unwrap();
    fs::create_dir_all(base.join("linkbin")).unwrap();
    Fixture { root: base.join("root"), repo, sandbox, _tmp: tmp }
}

impl Fixture {
    fn forge(&self, args: &[&str]) -> Output {
        self.forge_with(args, &[])
    }
    fn forge_with(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_forge"));
        c.env("FORGE_ROOT", &self.root)
            .env("FORGE_REPO", "")
            .env("FORGE_WAYLAND_SESSIONS_DIR", self.sessions())
            .env("FORGE_XSESSIONS_DIR", self.root.parent().unwrap().join("xsessions"))
            .env("FORGE_LINK_DIR", self.linkbin()) // never the real ~/.local/bin
            .args(args);
        c.envs(envs.iter().copied());
        if matches!(args[0], "install" | "build" | "update" | "upgrade" | "integrate" | "unintegrate") {
            c.arg("--yes");
            if !self.sandbox {
                c.arg("--insecure-no-sandbox");
            }
        }
        c.output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let o = self.forge(args);
        assert!(
            o.status.success(),
            "forge {args:?} failed:\n{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }
    fn fails(&self, args: &[&str]) -> String {
        let o = self.forge(args);
        assert!(!o.status.success(), "forge {args:?} should have failed");
        String::from_utf8_lossy(&o.stderr).into_owned()
    }
    fn linkbin(&self) -> PathBuf {
        self.root.parent().unwrap().join("linkbin")
    }
    fn sessions(&self) -> PathBuf {
        self.root.parent().unwrap().join("wayland-sessions")
    }
    fn repo_str(&self) -> &str {
        self.repo.to_str().unwrap()
    }
}

/// Installed state only: what a package can leave behind. Caches
/// (sources/builds/trash) and Forge's own database are not part of it.
fn installed_state(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for top in ["store", "current", "bin"] {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd {
                let p = e.unwrap().path();
                let rel = p.strip_prefix(root).unwrap().display().to_string();
                let m = fs::symlink_metadata(&p).unwrap();
                if m.is_dir() {
                    out.insert(rel, "dir".into());
                    walk(root, &p, out);
                } else if m.file_type().is_symlink() {
                    out.insert(rel, format!("link {}", fs::read_link(&p).unwrap().display()));
                } else {
                    out.insert(rel, format!("file {:o}", m.permissions().mode()));
                }
            }
        }
        walk(root, &root.join(top), &mut out);
    }
    out
}

#[test]
fn install_run_update_remove() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]); // creates the layout
    let before = installed_state(&f.root);

    let out = f.ok(&["build", f.repo_str(), "--name", "hello"]);
    assert!(out.contains("Installed hello 1.0.0-"), "{out}");
    assert!(out.contains("auto-detected") && out.contains("make"), "{out}");

    // The command works through Forge's managed symlink.
    let run = Command::new(f.root.join("bin/hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hello-v1\n");
    assert!(f.ok(&["list"]).contains("hello"));
    let info = f.ok(&["info", "hello"]);
    assert!(info.contains("Build system : make"), "{info}");
    assert!(info.contains("Executables  : hello"), "{info}");
    assert!(f.ok(&["verify"]).contains("intact"));

    // Installing twice is refused; update with nothing new is a no-op.
    assert!(f.ok(&["build", f.repo_str(), "--name", "hello"]).contains("already installed"));
    assert!(f.ok(&["update", "hello"]).contains("up to date"));

    // New commit -> update replaces the old version.
    fs::write(f.repo.join("hello.sh"), "#!/bin/sh\necho hello-v2\n").unwrap();
    commit_all(&f.repo, "v2");
    let out = f.ok(&["update", "hello"]);
    assert!(out.contains("Installed hello"), "{out}");
    let run = Command::new(f.root.join("bin/hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hello-v2\n");
    assert_eq!(fs::read_dir(f.root.join("store/hello")).unwrap().count(), 1, "old version not removed");
    assert!(f.ok(&["verify"]).contains("intact"));

    // Dry run touches nothing; the real remove restores the original state.
    let installed = installed_state(&f.root);
    assert!(f.ok(&["remove", "hello", "--dry-run"]).contains("Would remove"));
    assert_eq!(installed_state(&f.root), installed);
    f.ok(&["remove", "hello"]);
    assert_eq!(installed_state(&f.root), before);
    assert!(f.ok(&["list"]).contains("no packages"));
}

#[test]
fn pinned_revisions() {
    let f = fixture(MAKEFILE);
    fs::write(f.repo.join("hello.sh"), "#!/bin/sh\necho hello-v2\n").unwrap();
    commit_all(&f.repo, "v2");

    f.ok(&["build", f.repo_str(), "--name", "hello", "--rev", "v1.0.0"]);
    let run = Command::new(f.root.join("bin/hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hello-v1\n");
    // A tag-pinned package stays on its tag when updated.
    assert!(f.ok(&["update", "hello"]).contains("up to date"));
    f.ok(&["remove", "hello"]);

    assert!(f.fails(&["build", f.repo_str(), "--name", "hello", "--rev", "nonexistent"]).contains("resolving revision"));

    // A recipe-pinned commit that does not match is rejected before building.
    let recipe = f.root.parent().unwrap().join("pin.toml");
    fs::write(&recipe, "[package]\ncommit = \"deadbeef\"\n").unwrap();
    let err = f.fails(&["build", f.repo_str(), "--name", "hello", "--recipe", recipe.to_str().unwrap()]);
    assert!(err.contains("commit mismatch"), "{err}");
    assert!(installed_state(&f.root).is_empty() || !f.root.join("bin/hello").exists());
}

#[test]
fn build_that_writes_outside_its_prefix_is_rejected() {
    let evil = MAKEFILE.replace(
        "\tinstall -m644 hello.sh $(DESTDIR)$(PREFIX)/share/hello/source.sh\n",
        "\tinstall -D -m644 hello.sh $(DESTDIR)/etc/evil.conf\n",
    );
    let f = fixture(&evil);
    f.ok(&["list"]);
    let before = installed_state(&f.root);
    let err = f.fails(&["build", f.repo_str(), "--name", "hello"]);
    assert!(err.contains("outside its prefix") && err.contains("/etc/evil.conf"), "{err}");
    assert_eq!(installed_state(&f.root), before);
    assert!(f.ok(&["list"]).contains("no packages"));
}

#[test]
fn failed_build_installs_nothing() {
    let f = fixture("all:\n\tfalse\ninstall:\n\ttrue\n");
    f.ok(&["list"]);
    let before = installed_state(&f.root);
    let err = f.fails(&["build", f.repo_str(), "--name", "hello"]);
    assert!(err.contains("step failed"), "{err}");
    assert_eq!(installed_state(&f.root), before);
}

#[test]
fn custom_recipe_in_repository() {
    let f = fixture("");
    fs::remove_file(f.repo.join("Makefile")).unwrap();
    fs::write(
        f.repo.join("forge.toml"),
        r#"
[build]
steps = ['mkdir -p "$FORGE_STAGE$FORGE_PREFIX/bin"', 'cp hello.sh "$FORGE_STAGE$FORGE_PREFIX/bin/say-hello"', 'chmod +x "$FORGE_STAGE$FORGE_PREFIX/bin/say-hello"']
"#,
    )
    .unwrap();
    commit_all(&f.repo, "recipe");
    let out = f.ok(&["build", f.repo_str(), "--name", "hello"]);
    assert!(out.contains("forge.toml in the repository") && out.contains("custom"), "{out}");
    let run = Command::new(f.root.join("bin/say-hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hello-v1\n");
    f.ok(&["remove", "hello"]);
    assert!(!f.root.join("bin/say-hello").exists());
}

#[test]
fn undetectable_project_needs_a_recipe() {
    let f = fixture("");
    fs::remove_file(f.repo.join("Makefile")).unwrap();
    commit_all(&f.repo, "no build system");
    let err = f.fails(&["build", f.repo_str(), "--name", "hello"]);
    assert!(err.contains("could not detect a build system"), "{err}");
}

#[test]
fn missing_host_tool_is_reported_before_building() {
    let f = fixture(MAKEFILE);
    let recipe = f.root.parent().unwrap().join("tools.toml");
    fs::write(&recipe, "[build]\ntools = [\"definitely-not-a-real-tool\"]\npkgconfig = [\"no-such-lib >= 9\"]\n").unwrap();
    let err = f.fails(&["build", f.repo_str(), "--name", "hello", "--recipe", recipe.to_str().unwrap()]);
    assert!(err.contains("hello: tool definitely-not-a-real-tool") && err.contains("hello: pkg-config no-such-lib"), "{err}");
}

#[test]
fn sandbox_hides_the_host() {
    let f = fixture("");
    if !f.sandbox {
        eprintln!("skipping: bubblewrap unavailable");
        return;
    }
    fs::remove_file(f.repo.join("Makefile")).unwrap();
    // A file that exists on the host (in /tmp, which the sandbox replaces) and
    // the real home directory must both be invisible to the build.
    let marker = f.root.parent().unwrap().join("host-secret");
    fs::write(&marker, "secret").unwrap();
    let home = std::env::var("HOME").unwrap();
    let recipe = f.root.parent().unwrap().join("sandbox.toml");
    fs::write(
        &recipe,
        format!(
            r#"[build]
steps = [
  'test ! -e {marker}',
  'test ! -e {home}/.ssh && test ! -e {home}/.bashrc',
  'test "$HOME" = /build/home',
  'test "$(tail -n +3 /proc/net/dev | grep -vcE "^ *(lo|sit0|tunl0|ip6tnl0|gre0|gretap0|erspan0|ip6gre0|ip_vti0|ip6_vti0):")" = 0',
  'mkdir -p "$FORGE_STAGE$FORGE_PREFIX/bin" && echo ok > "$FORGE_STAGE$FORGE_PREFIX/bin/probe" && chmod +x "$FORGE_STAGE$FORGE_PREFIX/bin/probe"',
]
"#,
            marker = marker.display(),
            home = home
        ),
    )
    .unwrap();
    commit_all(&f.repo, "sandbox probe");
    let out = f.forge(&["build", f.repo_str(), "--name", "probe", "--recipe", recipe.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}


// ---- Forge packages depending on other Forge packages ----

const BASE_INSTALL: &str = r#"set -e
D="$FORGE_STAGE$FORGE_PREFIX"
mkdir -p "$D/bin" "$D/lib/pkgconfig"
printf '#!/bin/sh\necho base-%s\n' "$(cat version.txt)" > "$D/bin/base-tool"
chmod +x "$D/bin/base-tool"
printf 'Name: base\nDescription: b\nVersion: 1\nLibs: -L%s/lib\n' "$FORGE_PREFIX" > "$D/lib/pkgconfig/base.pc"
echo lib > "$D/lib/libbase.txt"
"#;

// The app proves it can see its dependency while building: on PATH, in
// FORGE_DEPS and through pkg-config. It records the dependency's path.
const APP_INSTALL: &str = r#"set -e
D="$FORGE_STAGE$FORGE_PREFIX"
tool="$(command -v base-tool)"
[ -n "$tool" ]
case "$FORGE_DEPS" in *base*) ;; *) echo "FORGE_DEPS=$FORGE_DEPS"; exit 1;; esac
pkg-config --exists base
mkdir -p "$D/bin"
printf '#!/bin/sh\necho app+$(%s)\n' "$tool" > "$D/bin/app"
chmod +x "$D/bin/app"
"#;

struct Deps {
    f: Fixture,
    base: PathBuf,
    app: PathBuf,
}

fn make_repo(parent: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let repo = parent.join(name);
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (n, c) in files {
        let f = repo.join(n);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, c).unwrap();
    }
    commit_all(&repo, "initial");
    repo
}

fn write_recipe(f: &Fixture, name: &str, repo: &Path, deps: &[&str], build_deps: &[&str], script: &str) {
    let q = |l: &[&str]| l.iter().map(|d| format!("\"{d}\"")).collect::<Vec<_>>().join(", ");
    fs::create_dir_all(f.root.join("recipes")).unwrap();
    fs::write(
        f.root.join(format!("recipes/{name}.toml")),
        format!(
            "[package]\nname = \"{name}\"\nrepo = \"{}\"\ndeps = [{}]\n[build]\nsteps = [\"{script}\"]\ndeps = [{}]\n",
            repo.display(),
            q(deps),
            q(build_deps)
        ),
    )
    .unwrap();
}

fn deps_fixture(app_script: &str) -> Option<Deps> {
    let f = fixture("");
    if !f.sandbox {
        eprintln!("skipping: dependencies on unbuilt packages need bubblewrap");
        return None;
    }
    let parent = f.repo.parent().unwrap().to_path_buf();
    let base = make_repo(&parent, "base", &[("install.sh", BASE_INSTALL), ("version.txt", "v1")]);
    let app = make_repo(&parent, "app", &[("install.sh", app_script)]);
    write_recipe(&f, "base", &base, &[], &[], "sh install.sh");
    write_recipe(&f, "app", &app, &["base"], &[], "sh install.sh");
    f.ok(&["list"]);
    Some(Deps { f, base, app })
}

fn run_bin(f: &Fixture, name: &str) -> String {
    let o = Command::new(f.root.join("bin").join(name)).output().unwrap();
    assert!(o.status.success(), "{name} failed");
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[test]
fn installing_a_package_builds_and_installs_its_dependencies_first() {
    let Some(d) = deps_fixture(APP_INSTALL) else { return };
    let f = &d.f;
    let before = installed_state(&f.root);

    let out = f.ok(&["install", "app"]);
    let (base_at, app_at) = (out.find("\n  base ").unwrap(), out.find("\n  app ").unwrap());
    assert!(base_at < app_at, "dependency must be built first:\n{out}");
    assert!(out.contains("dependency of app"), "{out}");
    assert!(out.contains("Installed base") && out.contains("[dependency]"), "{out}");

    // The app really ran against the dependency it saw while building.
    assert_eq!(run_bin(f, "app"), "app+base-v1");
    let list = f.ok(&["list"]);
    assert!(list.lines().any(|l| l.trim_start().starts_with("base") && l.contains("dependency")), "{list}");
    assert!(list.lines().any(|l| l.trim_start().starts_with("app") && l.contains("explicit")), "{list}");
    let info_app = f.ok(&["info", "app"]);
    assert!(info_app.contains("Forge deps   : base "), "{info_app}");
    assert!(info_app.lines().any(|l| l.trim_start().starts_with("Version") && l.contains("-d")), "version id carries a dependency tag:\n{info_app}");
    assert!(f.ok(&["info", "base"]).contains("Required by  : app"));
    assert!(f.ok(&["verify"]).contains("intact"));

    // A needed package cannot be removed from under its dependents.
    assert!(f.fails(&["remove", "base"]).contains("required by: app"));

    // Dry run shows the whole cascade and changes nothing.
    let installed = installed_state(&f.root);
    let dry = f.ok(&["remove", "app", "--dry-run"]);
    assert!(dry.contains("Would remove app") && dry.contains("Would remove base") && dry.contains("no longer needed"), "{dry}");
    assert_eq!(installed_state(&f.root), installed);

    // Removing the app removes the dependency nobody else needs: back to square one.
    let out = f.ok(&["remove", "app"]);
    assert!(out.contains("Removed app") && out.contains("Removed base"), "{out}");
    assert_eq!(installed_state(&f.root), before);
}

#[test]
fn explicitly_installed_dependencies_are_kept() {
    let Some(d) = deps_fixture(APP_INSTALL) else { return };
    let f = &d.f;
    let before = installed_state(&f.root);

    // Install base on purpose first: removing the app must not take it along.
    f.ok(&["install", "base"]);
    f.ok(&["install", "app"]);
    f.ok(&["remove", "app"]);
    assert!(f.ok(&["list"]).contains("base"));
    f.ok(&["remove", "base"]);
    assert_eq!(installed_state(&f.root), before);

    // Asking for an automatically installed dependency promotes it.
    f.ok(&["install", "app"]);
    let out = f.ok(&["install", "base"]);
    assert!(out.contains("now kept explicitly"), "{out}");
    f.ok(&["remove", "app"]);
    assert!(f.ok(&["list"]).contains("base"), "promoted dependency must survive");
    f.ok(&["remove", "base"]);
    assert_eq!(installed_state(&f.root), before);
}

#[test]
fn keep_deps_and_autoremove() {
    let Some(d) = deps_fixture(APP_INSTALL) else { return };
    let f = &d.f;
    let before = installed_state(&f.root);
    f.ok(&["install", "app"]);
    f.ok(&["remove", "app", "--keep-deps"]);
    assert!(f.ok(&["list"]).contains("base"));
    assert!(f.ok(&["autoremove", "--dry-run"]).contains("Would remove base"));
    assert!(f.ok(&["list"]).contains("base"));
    f.ok(&["autoremove"]);
    assert_eq!(installed_state(&f.root), before);
    assert!(f.ok(&["autoremove"]).contains("no unneeded dependencies"));
}

#[test]
fn a_failing_build_leaves_no_dependencies_behind() {
    let Some(d) = deps_fixture(&format!("{APP_INSTALL}exit 1\n")) else { return };
    let f = &d.f;
    let before = installed_state(&f.root);
    let err = f.fails(&["install", "app"]);
    assert!(err.contains("Nothing was installed"), "{err}");
    // The dependency built fine, but must not stay installed without its dependent.
    assert_eq!(installed_state(&f.root), before);
    assert!(f.ok(&["list"]).contains("no packages"));
}

#[test]
fn updating_a_dependency_rebuilds_its_dependents() {
    let Some(d) = deps_fixture(APP_INSTALL) else { return };
    let f = &d.f;
    let before = installed_state(&f.root);
    f.ok(&["install", "app"]);
    assert_eq!(run_bin(f, "app"), "app+base-v1");
    let version = |pkg: &str| {
        f.ok(&["info", pkg]).lines().find(|l| l.trim_start().starts_with("Version")).unwrap().to_string()
    };
    let (app_v1, base_v1) = (version("app"), version("base"));

    // Nothing changed: nothing to do.
    assert!(f.ok(&["update", "app"]).contains("up to date"));
    assert!(f.ok(&["upgrade"]).contains("up to date"));

    fs::write(d.base.join("version.txt"), "v2").unwrap();
    commit_all(&d.base, "v2");
    let out = f.ok(&["update", "base"]);
    assert!(out.contains("rebuild: a dependency was updated"), "{out}");
    assert!(out.contains("Installed base") && out.contains("Installed app"), "{out}");

    assert_ne!(version("base"), base_v1);
    assert_ne!(version("app"), app_v1, "the dependent must get a new version id");
    assert_eq!(run_bin(f, "app"), "app+base-v2", "app must use the new dependency");
    assert_eq!(fs::read_dir(f.root.join("store/base")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(f.root.join("store/app")).unwrap().count(), 1);
    assert!(f.ok(&["verify"]).contains("intact"));

    // And the whole thing still comes out clean.
    f.ok(&["remove", "app"]);
    assert_eq!(installed_state(&f.root), before);
}

#[test]
fn upgrade_handles_a_changed_dependency_and_a_changed_dependent_together() {
    let Some(d) = deps_fixture(APP_INSTALL) else { return };
    let f = &d.f;
    f.ok(&["install", "app"]);
    fs::write(d.base.join("version.txt"), "v2").unwrap();
    commit_all(&d.base, "v2");
    fs::write(d.app.join("note.txt"), "changed").unwrap();
    commit_all(&d.app, "app change");
    f.ok(&["upgrade"]);
    assert_eq!(run_bin(f, "app"), "app+base-v2");
    assert!(f.ok(&["verify"]).contains("intact"));
    assert_eq!(fs::read_dir(f.root.join("store/app")).unwrap().count(), 1);
}

#[test]
fn build_only_dependencies_are_visible_while_building_and_tracked() {
    let Some(d) = deps_fixture(APP_INSTALL) else { return };
    let f = &d.f;
    write_recipe(f, "app", &d.app, &[], &["base"], "sh install.sh");
    f.ok(&["install", "app"]);
    assert_eq!(run_bin(f, "app"), "app+base-v1");
    assert!(f.ok(&["info", "app"]).contains("base"), "build-only dep recorded");
    assert!(f.ok(&["info", "app"]).contains("(build only)"));
    assert!(f.fails(&["remove", "base"]).contains("required by: app"));
}

#[test]
fn missing_recipe_and_cycles_are_reported_clearly() {
    let Some(d) = deps_fixture(APP_INSTALL) else { return };
    let f = &d.f;
    let before = installed_state(&f.root);
    write_recipe(f, "app", &d.app, &["no-such-package"], &[], "sh install.sh");
    let err = f.fails(&["install", "app"]);
    assert!(err.contains("needs 'no-such-package'") && err.contains("no recipe"), "{err}");

    write_recipe(f, "app", &d.app, &["base"], &[], "sh install.sh");
    write_recipe(f, "base", &d.base, &["app"], &[], "sh install.sh");
    let err = f.fails(&["install", "app"]);
    assert!(err.contains("dependency cycle: app -> base -> app"), "{err}");
    assert_eq!(installed_state(&f.root), before);
}


// ---- forge integrate: session files for display managers ----

fn wm_install_script(desktop: &str) -> String {
    format!(
        r#"set -e
D="$FORGE_STAGE$FORGE_PREFIX"
mkdir -p "$D/bin" "$D/share/wayland-sessions"
printf '#!/bin/sh\necho mywm-%s\n' "$(cat version.txt)" > "$D/bin/mywm"
chmod +x "$D/bin/mywm"
{desktop}
"#
    )
}

const WM_DESKTOP: &str =
    "printf '[Desktop Entry]\\nName=MyWM\\nExec=mywm --start\\nType=Application\\n' > \"$D/share/wayland-sessions/mywm.desktop\"";

fn wm_fixture(install_script: &str) -> Fixture {
    let f = fixture("");
    fs::remove_file(f.repo.join("Makefile")).unwrap();
    fs::write(f.repo.join("install.sh"), install_script).unwrap();
    fs::write(f.repo.join("version.txt"), "v1").unwrap();
    fs::write(f.repo.join("forge.toml"), "[build]\nsteps = [\"sh install.sh\"]\n").unwrap();
    commit_all(&f.repo, "wm");
    f.ok(&["build", f.repo_str(), "--name", "wm"]);
    f
}

fn dir_names(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn integrate_registers_a_working_session_and_remove_cleans_it_up() {
    let f = wm_fixture(&wm_install_script(WM_DESKTOP));
    let sessions = f.sessions();
    let dest = sessions.join("forge-mywm.desktop");
    let wrapper = f.root.join("integration/wm/mywm-session");

    // Dry run shows the file and installs nothing.
    let out = f.ok(&["integrate", "wm", "--dry-run"]);
    assert!(out.contains("Name=MyWM (Forge)") && out.contains("forge-mywm.desktop"), "{out}");
    assert!(dir_names(&sessions).is_empty() && !f.root.join("integration").exists());

    let out = f.ok(&["integrate", "wm"]);
    assert!(out.contains("Installed") && out.contains("forge-mywm.desktop"), "{out}");
    let desktop = fs::read_to_string(&dest).unwrap();
    assert!(desktop.contains("Name=MyWM (Forge)\n"), "{desktop}");
    assert!(desktop.contains(&format!("Exec={} --start\n", wrapper.display())), "{desktop}");
    assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o644);

    // The wrapper runs the package's program with Forge's bin on PATH.
    let o = Command::new(&wrapper).arg("--start").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "mywm-v1");
    let script = fs::read_to_string(&wrapper).unwrap();
    assert!(script.contains(&format!("PATH='{}'", f.root.join("bin").display())), "{script}");

    assert!(f.ok(&["info", "wm"]).contains(&format!("Integrated   : {}", dest.display())));
    assert!(f.ok(&["verify"]).contains("intact"));

    // It survives an update and keeps working (the wrapper goes via stable links).
    fs::write(f.repo.join("version.txt"), "v2").unwrap();
    commit_all(&f.repo, "v2");
    f.ok(&["update", "wm"]);
    let o = Command::new(&wrapper).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "mywm-v2");
    assert!(f.ok(&["verify"]).contains("intact"));

    // Removing needs confirmation for system files; dry run changes nothing.
    assert!(f.fails(&["remove", "wm"]).contains("pass --yes"));
    assert!(dest.exists());
    assert!(f.ok(&["remove", "wm", "--dry-run"]).contains("Would remove system file"));
    assert!(dest.exists());

    let out = f.ok(&["remove", "wm", "--yes"]);
    assert!(out.contains("Removed system file"), "{out}");
    assert!(dir_names(&sessions).is_empty(), "session file left behind");
    assert!(!f.root.join("integration").exists(), "wrapper left behind");
    assert!(f.ok(&["list"]).contains("no packages"));
    assert!(installed_state(&f.root).values().all(|v| v == "dir"), "{:?}", installed_state(&f.root));
}

#[test]
fn unintegrate_keeps_the_package_and_modified_files_are_never_deleted() {
    let f = wm_fixture(&wm_install_script(WM_DESKTOP));
    let dest = f.sessions().join("forge-mywm.desktop");
    f.ok(&["integrate", "wm"]);
    f.ok(&["unintegrate", "wm"]);
    assert!(!dest.exists() && !f.root.join("integration").exists());
    assert!(f.ok(&["list"]).contains("wm"));
    assert!(f.ok(&["unintegrate", "wm"]).contains("no integrated files"));

    // Someone edits the installed file: removal leaves it alone and says so.
    f.ok(&["integrate", "wm"]);
    fs::write(&dest, "[Desktop Entry]\nName=mine\n").unwrap();
    let o = f.forge(&["verify"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("integrated file was modified"));
    let o = f.forge(&["remove", "wm", "--yes"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("no longer the file Forge installed"));
    assert_eq!(fs::read_to_string(&dest).unwrap(), "[Desktop Entry]\nName=mine\n");
    assert!(f.ok(&["list"]).contains("no packages"));
}

#[test]
fn integrate_refuses_unowned_files_and_unusable_session_files() {
    let f = wm_fixture(&wm_install_script(WM_DESKTOP));
    let dest = f.sessions().join("forge-mywm.desktop");
    fs::write(&dest, "someone else's").unwrap();
    let err = f.fails(&["integrate", "wm"]);
    assert!(err.contains("not managed by Forge"), "{err}");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "someone else's");
    assert!(!f.root.join("integration").exists());
    f.ok(&["remove", "wm"]); // nothing was integrated, so no confirmation needed

    // A session whose command the package does not provide.
    let f = wm_fixture(&wm_install_script(&WM_DESKTOP.replace("Exec=mywm --start", "Exec=mywm-session --start")));
    let err = f.fails(&["integrate", "wm"]);
    assert!(err.contains("'mywm-session'") && err.contains("does not provide"), "{err}");
    assert!(dir_names(&f.sessions()).is_empty());

    // A package with no session files at all.
    let f = wm_fixture(&wm_install_script("true"));
    assert!(f.fails(&["integrate", "wm"]).contains("ships no session files"));
}

#[test]
fn integrate_requires_an_installed_package_and_valid_overrides() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]);
    assert!(f.fails(&["integrate", "nope"]).contains("not installed"));
    // The directory override is only honoured when no sudo would be needed.
    let wm = wm_fixture(&wm_install_script(WM_DESKTOP));
    let o = Command::new(env!("CARGO_BIN_EXE_forge"))
        .env("FORGE_ROOT", &wm.root)
        .env("FORGE_REPO", "")
        .env("FORGE_WAYLAND_SESSIONS_DIR", "/usr")
        .env("FORGE_LINK_DIR", "")
        .args(["integrate", "wm", "--yes"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("writable without sudo"));
}

// ---- isolation: a program's files live inside the package's data directory ----

const PROBE_INSTALL: &str = r#"set -e
D="$FORGE_STAGE$FORGE_PREFIX"
mkdir -p "$D/bin" "$D/share/probe"
printf '#!/bin/sh\necho "cfg=${XDG_CONFIG_HOME-unset} extra=${EXTRA-unset}"\nmkdir -p "${XDG_CONFIG_HOME:-/nonexistent-no-write}/probe" 2>/dev/null && echo written > "$XDG_CONFIG_HOME/probe/settings"\n' > "$D/bin/probe"
chmod +x "$D/bin/probe"
echo "default-settings" > "$D/share/probe/default.conf"
"#;

fn probe_fixture(forge_toml: &str) -> Fixture {
    let f = fixture("");
    fs::remove_file(f.repo.join("Makefile")).unwrap();
    fs::write(f.repo.join("install.sh"), PROBE_INSTALL).unwrap();
    fs::write(f.repo.join("forge.toml"), format!("[build]\nsteps = [\"sh install.sh\"]\n{forge_toml}")).unwrap();
    commit_all(&f.repo, "probe");
    f
}

fn run_probe(f: &Fixture) -> String {
    let o = Command::new(f.root.join("bin/probe")).env_remove("XDG_CONFIG_HOME").env_remove("EXTRA").output().unwrap();
    String::from_utf8_lossy(&o.stdout).to_string()
}

#[test]
fn programs_keep_their_files_inside_the_package_by_default() {
    let f = probe_fixture("");
    let out = f.ok(&["build", f.repo_str(), "--name", "probe"]);
    assert!(out.contains("config, data, cache and state kept in"), "{out}");
    let data = f.root.join("data/probe");

    // The program's configuration lands in the package's data directory, not ~/.config.
    assert!(run_probe(&f).starts_with(&format!("cfg={}/config ", data.display())));
    assert_eq!(fs::read_to_string(data.join("config/probe/settings")).unwrap().trim(), "written");
    let info = f.ok(&["info", "probe"]);
    assert!(info.contains("Isolation    : xdg") && info.contains(&format!("Data dir     : {}", data.display())), "{info}");

    // Files the program created are listed and need a confirmation to delete.
    let err = f.fails(&["remove", "probe"]);
    assert!(err.contains("pass --yes"), "{err}");
    assert!(f.ok(&["remove", "probe", "--dry-run"]).contains(&format!("Would remove data directory {}", data.display())));
    assert!(data.join("config/probe/settings").exists(), "dry run and refusal must delete nothing");

    // --keep-data keeps the user's files (e.g. to reinstall later); a plain removal deletes them.
    f.ok(&["remove", "probe", "--yes", "--keep-data"]);
    assert!(data.join("config/probe/settings").exists() && !f.root.join("bin/probe").exists());
    f.ok(&["build", f.repo_str(), "--name", "probe"]);
    assert!(run_probe(&f).starts_with("cfg="), "reinstalled package picks its old data up again");
    assert_eq!(fs::read_to_string(data.join("config/probe/settings")).unwrap().trim(), "written");
    let out = f.ok(&["remove", "probe", "--yes"]);
    assert!(out.contains("data directory"), "{out}");
    assert!(!f.root.join("data").exists(), "everything the program created is gone");
    assert!(installed_state(&f.root).values().all(|v| v == "dir"));
}

#[test]
fn isolation_can_be_turned_off_and_updates_keep_user_files() {
    let f = probe_fixture("");
    f.ok(&["build", f.repo_str(), "--name", "probe", "--isolate", "none"]);
    assert_eq!(fs::read_link(f.root.join("bin/probe")).unwrap(), Path::new("../current/probe/bin/probe"));
    assert!(run_probe(&f).starts_with("cfg=unset"));
    assert!(!f.root.join("data").exists());
    f.ok(&["remove", "probe"]);

    // Default isolation survives updates, along with what the program wrote.
    f.ok(&["build", f.repo_str(), "--name", "probe"]);
    run_probe(&f);
    fs::write(f.root.join("data/probe/config/user.conf"), "mine").unwrap();
    fs::write(f.repo.join("note.txt"), "x").unwrap();
    commit_all(&f.repo, "update");
    f.ok(&["update", "probe"]);
    assert!(run_probe(&f).starts_with("cfg="));
    assert_eq!(fs::read_to_string(f.root.join("data/probe/config/user.conf")).unwrap(), "mine");
    assert!(f.ok(&["verify"]).contains("intact"));
    f.ok(&["remove", "probe", "--yes"]);
}

#[test]
fn recipes_can_set_targeted_variables_and_seed_default_config() {
    // The compositor style: leave the environment alone, point only the
    // program's own setting at the package, and provide a default config.
    let f = probe_fixture(
        r#"[run]
isolate = "none"
dirs = ["config/probe"]
[run.env]
EXTRA = "{data}/config/probe/main.conf"
[run.seed]
"config/probe/main.conf" = "share/probe/default.conf"
"#,
    );
    let out = f.ok(&["build", f.repo_str(), "--name", "probe"]);
    assert!(out.contains("environment left alone; sets EXTRA; default files: config/probe/main.conf"), "{out}");
    let data = f.root.join("data/probe");
    let out = run_probe(&f);
    assert_eq!(out.lines().next().unwrap(), format!("cfg=unset extra={}/config/probe/main.conf", data.display()));
    assert_eq!(fs::read_to_string(data.join("config/probe/main.conf")).unwrap().trim(), "default-settings");

    // The user's edits to the seeded file are never overwritten.
    fs::write(data.join("config/probe/main.conf"), "edited").unwrap();
    run_probe(&f);
    assert_eq!(fs::read_to_string(data.join("config/probe/main.conf")).unwrap(), "edited");
    f.ok(&["remove", "probe", "--yes"]);
    assert!(!f.root.join("data").exists());
}

#[test]
fn an_invalid_isolation_setting_is_rejected_before_building() {
    let f = probe_fixture("[run]\nisolate = \"sometimes\"\n");
    f.ok(&["list"]);
    let err = f.fails(&["build", f.repo_str(), "--name", "probe"]);
    assert!(err.contains("run.isolate"), "{err}");
    assert!(f.ok(&["list"]).contains("no packages"));
}


// ---- taps: recipe repositories, and git URLs that resolve to their recipes ----

fn tap_recipe(name: &str, repo_url: &str, deps: &str, extra: &str) -> String {
    format!("[package]\nname = \"{name}\"\nrepo = \"{repo_url}\"\ndeps = [{deps}]\n[build]\nsteps = [\"sh install.sh\"]\n{extra}")
}

struct Tapped {
    f: Fixture,
    app_url: String,
    app: PathBuf,
    tap: PathBuf,
}

fn tap_fixture() -> Option<Tapped> {
    let f = fixture("");
    if !f.sandbox {
        eprintln!("skipping: dependencies on unbuilt packages need bubblewrap");
        return None;
    }
    let parent = f.repo.parent().unwrap().to_path_buf();
    let base = make_repo(&parent, "base", &[("install.sh", BASE_INSTALL), ("version.txt", "v1")]);
    let app = make_repo(&parent, "app", &[("install.sh", APP_INSTALL)]);
    let (base_url, app_url) = (format!("file://{}", base.display()), format!("file://{}", app.display()));
    let tap = make_repo(
        &parent,
        "recipes-repo",
        &[
            ("recipes/base.toml", &tap_recipe("base", &base_url, "", "")),
            ("recipes/app.toml", &tap_recipe("app", &app_url, "\"base\"", "")),
        ],
    );
    f.ok(&["list"]);
    Some(Tapped { f, app_url, app, tap })
}

#[test]
fn a_git_url_described_by_a_tap_uses_the_tap_recipe_and_its_dependencies() {
    let Some(t) = tap_fixture() else { return };
    let f = &t.f;
    let before = installed_state(&f.root);

    let out = f.ok(&["tap", "add", t.tap.to_str().unwrap(), "--name", "mytap"]);
    assert!(out.contains("Added 'mytap' with 2 recipes"), "{out}");
    let listing = f.ok(&["tap", "list"]);
    assert!(listing.contains("mytap") && listing.contains("2 curated"), "{listing}");
    let found = f.ok(&["search", "app"]);
    assert!(found.lines().any(|l| l.trim_start().starts_with("app")), "{found}");

    // Installing by URL picks up the recipe, including its dependency on `base`.
    let out = f.ok(&["build", &t.app_url]);
    assert!(out.contains("Using the recipe 'app' from 'mytap'"), "{out}");
    assert!(out.contains("dependency of app") && out.contains("Installed base"), "{out}");
    assert_eq!(run_bin(f, "app"), "app+base-v1");
    f.ok(&["remove", "app"]);
    assert_eq!(installed_state(&f.root), before);

    // Other spellings of the same URL match too; by name works as before.
    f.ok(&["build", &format!("{}/", t.app_url)]);
    f.ok(&["remove", "app"]);
    f.ok(&["install", "app"]);
    f.ok(&["remove", "app"]);
    assert_eq!(installed_state(&f.root), before);

    // --ignore-taps builds the repository with no recipe at all (and here that cannot work).
    let err = f.fails(&["build", &t.app_url, "--ignore-repo"]);
    assert!(err.contains("could not detect a build system"), "{err}");
}

#[test]
fn repositories_no_tap_describes_still_install_on_their_own() {
    let Some(t) = tap_fixture() else { return };
    let f = &t.f;
    f.ok(&["tap", "add", t.tap.to_str().unwrap()]);
    fs::write(f.repo.join("Makefile"), MAKEFILE).unwrap();
    commit_all(&f.repo, "real makefile");
    let plain = format!("file://{}", f.repo.display());
    let before = installed_state(&f.root);
    let out = f.ok(&["build", &plain, "--name", "hello"]);
    assert!(out.contains("recipe") && out.contains("auto-detected") && !out.contains("Using the recipe"), "{out}");
    assert!(out.contains("auto-detected") && out.contains("make"), "{out}");
    f.ok(&["remove", "hello"]);
    assert_eq!(installed_state(&f.root), before);
}

#[test]
fn your_own_recipes_take_priority_over_taps() {
    let Some(t) = tap_fixture() else { return };
    let f = &t.f;
    f.ok(&["tap", "add", t.tap.to_str().unwrap()]);
    fs::create_dir_all(f.root.join("recipes")).unwrap();
    fs::write(
        f.root.join("recipes/app.toml"),
        tap_recipe("app", &t.app_url, "", "[install]\nexports = [\"bin/mine\"]\n"),
    )
    .unwrap();
    // The local recipe has no dependency on base, so the build cannot see it and fails;
    // what matters here is which recipe was chosen.
    let o = f.forge(&["install", "app"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("recipes/app.toml") && !out.contains("tap '"), "{out}");
    assert!(!out.contains("dependency of app"), "{out}");
}

#[test]
fn tap_updates_bring_in_recipe_changes_and_refresh_recipe_applies_them_to_updates() {
    let Some(t) = tap_fixture() else { return };
    let f = &t.f;
    f.ok(&["tap", "add", t.tap.to_str().unwrap(), "--name", "mytap"]);
    let out = f.ok(&["install", "app"]);
    assert!(out.contains("kept in"), "default isolation first: {out}");
    let upd = f.ok(&["tap", "update"]);
    assert!(upd.contains("mytap") && upd.contains("up to date"), "{upd}");

    // The recipe author changes the recipe and the project gets a new commit.
    fs::write(t.tap.join("recipes/app.toml"), tap_recipe("app", &t.app_url, "\"base\"", "[run]\nisolate = \"none\"\n")).unwrap();
    commit_all(&t.tap, "app: no isolation");
    let changed = f.ok(&["tap", "update"]);
    assert!(changed.contains("mytap") && !changed.contains("up to date"), "{changed}");
    assert!(f.ok(&["tap", "update"]).contains("up to date"), "second update has nothing to fetch");
    fs::write(t.app.join("note.txt"), "x").unwrap();
    commit_all(&t.app, "new commit");

    // A plain update keeps the recipe saved at install time...
    let out = f.ok(&["update", "app"]);
    assert!(out.contains("recipe saved at install time") && out.contains("kept in"), "{out}");

    // ...and --refresh-recipe takes the tap's current one.
    fs::write(t.app.join("note.txt"), "y").unwrap();
    commit_all(&t.app, "another commit");
    let out = f.ok(&["update", "app", "--refresh-recipe"]);
    assert!(out.contains("'mytap'") && out.contains("environment left alone"), "{out}");
    assert!(f.ok(&["verify"]).contains("intact"));
    assert!(f.ok(&["info", "app"]).contains("Isolation    : none"));
}

#[test]
fn tap_management_errors_and_removal() {
    let f = fixture("");
    f.ok(&["list"]);
    let parent = f.repo.parent().unwrap().to_path_buf();
    let empty = make_repo(&parent, "empty-tap", &[("README.md", "no recipes here")]);
    let err = f.fails(&["tap", "add", empty.to_str().unwrap()]);
    assert!(err.contains("has no recipes"), "{err}");
    assert!(!f.root.join("taps/empty-tap").exists(), "a failed add leaves nothing behind");

    let tap = make_repo(&parent, "good-tap", &[("recipes/hello.toml", &tap_recipe("hello", &format!("file://{}", f.repo.display()), "", ""))]);
    f.ok(&["tap", "add", tap.to_str().unwrap()]);
    assert!(f.fails(&["tap", "add", tap.to_str().unwrap()]).contains("already exists"));
    assert!(f.fails(&["tap", "update", "nope"]).contains("no tap named"));
    assert!(f.fails(&["tap", "remove", "../x"]).contains("invalid"));

    f.ok(&["tap", "remove", "good-tap"]);
    assert!(!f.root.join("taps/good-tap").exists());
    assert!(f.ok(&["tap", "list"]).contains("no repositories"));
    let err = f.fails(&["install", "hello"]);
    assert!(err.contains("no package named 'hello'"), "{err}");
}

// ---- commands work right after install: linked into a directory already on PATH ----

#[test]
fn installed_commands_are_linked_into_the_path_directory_automatically() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]);
    let link = f.linkbin().join("hello");
    let out = f.ok(&["build", f.repo_str(), "--name", "hello"]);
    assert!(out.contains(&format!("linked into {}", f.linkbin().display())), "{out}");

    // Runnable through the PATH directory straight away.
    let run = Command::new(&link).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hello-v1\n");
    assert_eq!(fs::read_link(&link).unwrap(), f.root.join("bin/hello"));
    assert!(f.ok(&["verify"]).contains("intact"));
    assert!(f.ok(&["info", "hello"]).contains(&format!("Link         : {}", link.display())));

    // An update keeps it working; removal deletes it.
    fs::write(f.repo.join("hello.sh"), "#!/bin/sh\necho hello-v2\n").unwrap();
    commit_all(&f.repo, "v2");
    f.ok(&["update", "hello"]);
    assert_eq!(String::from_utf8_lossy(&Command::new(&link).output().unwrap().stdout), "hello-v2\n");
    f.ok(&["remove", "hello"]);
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(fs::read_dir(f.linkbin()).unwrap().next().is_none(), "nothing left in the PATH directory");
}

#[test]
fn a_users_own_file_in_the_path_directory_is_left_alone() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]);
    let mine = f.linkbin().join("hello");
    fs::write(&mine, "mine").unwrap();
    let o = f.forge(&["build", f.repo_str(), "--name", "hello"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("not managed by Forge; left alone"));
    assert_eq!(fs::read_to_string(&mine).unwrap(), "mine");
    assert!(f.root.join("bin/hello").exists(), "still available through Forge's own bin");
    f.ok(&["remove", "hello"]);
    assert_eq!(fs::read_to_string(&mine).unwrap(), "mine");
}

#[test]
fn linking_can_be_disabled_with_an_empty_link_dir() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]);
    let o = Command::new(env!("CARGO_BIN_EXE_forge"))
        .env("FORGE_ROOT", &f.root)
        .env("FORGE_REPO", "")
        .env("FORGE_LINK_DIR", "")
        .args(["build", f.repo_str(), "--name", "hello", "--yes"])
        .arg(if f.sandbox { "--keep-build" } else { "--insecure-no-sandbox" })
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(fs::read_dir(f.linkbin()).unwrap().next().is_none());
    assert!(f.root.join("bin/hello").exists());
    assert!(String::from_utf8_lossy(&o.stdout).contains("add ") || String::from_utf8_lossy(&o.stdout).contains("PATH"));
}

#[test]
fn relink_repairs_a_package_installed_without_a_path_link() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]);
    // Installed with linking disabled, as older versions did.
    let o = Command::new(env!("CARGO_BIN_EXE_forge"))
        .env("FORGE_ROOT", &f.root)
        .env("FORGE_REPO", "")
        .env("FORGE_LINK_DIR", "")
        .args(["build", f.repo_str(), "--name", "hello", "--yes"])
        .arg(if f.sandbox { "--keep-build" } else { "--insecure-no-sandbox" })
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(fs::read_dir(f.linkbin()).unwrap().next().is_none());

    let out = f.ok(&["relink"]);
    assert!(out.contains("1 link(s) created"), "{out}");
    let run = Command::new(f.linkbin().join("hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hello-v1\n");
    assert!(f.ok(&["relink", "hello"]).contains("0 link(s) created"));
    assert!(f.ok(&["verify"]).contains("intact"));
    f.ok(&["remove", "hello"]);
    assert!(fs::read_dir(f.linkbin()).unwrap().next().is_none());
}


// ---- how the install looks ----

#[test]
fn output_is_plain_text_when_not_on_a_terminal_and_colored_only_on_request() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]);
    let out = f.ok(&["build", f.repo_str(), "--name", "hello"]);
    assert!(!out.contains('\x1b'), "no escape codes when output is not a terminal: {out:?}");
    // Compact plan, aligned steps with timings, a summary.
    assert!(out.contains("Installing 1 package"), "{out}");
    assert!(out.contains("source ") && out.contains("recipe ") && out.contains("runtime "), "{out}");
    for step in ["compile", "stage files", "install"] {
        assert!(out.lines().any(|l| l.contains(step) && l.trim_end().ends_with('s')), "{step}:\n{out}");
    }
    assert!(out.contains("Installed hello 1.0.0-") && out.contains("done in "), "{out}");
    // The raw command lines are not repeated by default.
    assert!(!out.contains("FORGE_OBJ") && !out.contains("make -j"), "{out}");
    f.ok(&["remove", "hello"]);

    // FORGE_COLOR=always colors it even in a pipe; FORGE_ASCII swaps the symbols.
    let colored = Command::new(env!("CARGO_BIN_EXE_forge"))
        .env("FORGE_ROOT", &f.root)
        .env("FORGE_REPO", "")
        .env("FORGE_LINK_DIR", f.linkbin())
        .env("FORGE_COLOR", "always")
        .env("FORGE_ASCII", "1")
        .args(["build", f.repo_str(), "--name", "hello", "--yes"])
        .arg(if f.sandbox { "--keep-build" } else { "--insecure-no-sandbox" })
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&colored.stdout);
    assert!(out.contains("\x1b[32m") && out.contains("\x1b[1m"), "{out:?}");
    assert!(!out.contains('✓') && out.contains("ok"), "ASCII symbols: {out:?}");
}

#[test]
fn details_lists_every_command_and_recipe_commands_are_always_shown() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]);
    let default = f.ok(&["build", f.repo_str(), "--name", "hello", "--details"]);
    assert!(default.contains("make -j") && default.contains("make install"), "{default}");
    f.ok(&["remove", "hello"]);

    // Commands supplied by a recipe are the text worth reviewing: shown even without --details.
    let r = f.root.parent().unwrap().join("custom.toml");
    fs::write(&r, "[build]\nsteps = [\"make\", \"make install PREFIX=$FORGE_PREFIX DESTDIR=$FORGE_STAGE\"]\n").unwrap();
    let out = f.ok(&["build", f.repo_str(), "--name", "hello", "--recipe", r.to_str().unwrap()]);
    assert!(out.contains("runs ") && out.contains("$ make install PREFIX=$FORGE_PREFIX"), "{out}");
    assert!(!out.contains("(--details lists every command)") || out.contains("runs"), "{out}");
}

#[test]
fn without_yes_a_non_interactive_install_refuses_instead_of_guessing() {
    let f = fixture(MAKEFILE);
    f.ok(&["list"]);
    let o = Command::new(env!("CARGO_BIN_EXE_forge"))
        .env("FORGE_ROOT", &f.root)
        .env("FORGE_REPO", "")
        .env("FORGE_LINK_DIR", f.linkbin())
        .args(["build", f.repo_str(), "--name", "hello"])
        .arg(if f.sandbox { "--keep-build" } else { "--insecure-no-sandbox" })
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("pass --yes"));
    assert!(f.ok(&["list"]).contains("no packages"));
}

#[test]
fn missing_host_libraries_of_the_whole_tree_are_reported_before_anything_is_downloaded() {
    let Some(d) = deps_fixture(APP_INSTALL) else { return };
    let f = &d.f;
    // `base` (a dependency of `app`) needs a tool this machine doesn't have.
    fs::write(
        f.root.join("recipes/base.toml"),
        format!(
            "[package]\nname = \"base\"\nrepo = \"{}\"\n[build]\nsteps = [\"sh install.sh\"]\ntools = [\"no-such-tool\"]\npkgconfig = [\"no-such-lib\"]\n",
            d.base.display()
        ),
    )
    .unwrap();
    let err = f.fails(&["install", "app"]);
    assert!(err.contains("base: tool no-such-tool") && err.contains("base: pkg-config no-such-lib"), "{err}");
    assert!(
        !f.root.join("sources").exists() || fs::read_dir(f.root.join("sources")).unwrap().next().is_none(),
        "nothing may be downloaded before the host check passes"
    );
    assert!(f.ok(&["list"]).contains("no packages"));
}


// ---- toolchains Forge provides itself (Go, Rust) ----

struct Release {
    dir: PathBuf,
    envs: Vec<(String, String)>,
}

fn sha256(path: &Path) -> String {
    let o = Command::new("sha256sum").arg(path).output().unwrap();
    String::from_utf8_lossy(&o.stdout).split_whitespace().next().unwrap().to_string()
}

fn run_ok(c: &mut Command) {
    let o = c.output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

/// A fake "go.dev" on disk: an index plus a tarball holding a fake `go`.
fn fake_go_release(parent: &Path, version: &str, tamper: bool) -> Option<Release> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    let dir = parent.join("fake-go-dl");
    let tree = dir.join("src/go/bin");
    fs::create_dir_all(&tree).unwrap();
    fs::write(
        tree.join("go"),
        format!(
            r#"#!/bin/sh
case "$1" in
  version) echo "go version go{version} linux/{arch}" ;;
  mod) exit 0 ;;
  install) mkdir -p "$GOBIN"; printf '#!/bin/sh\necho built-with-managed-go toolchain=%s\n' "$GOTOOLCHAIN" > "$GOBIN/hello"; chmod +x "$GOBIN/hello" ;;
  *) echo "unexpected: $*" >&2; exit 1 ;;
esac
"#
        ),
    )
    .unwrap();
    fs::set_permissions(tree.join("go"), fs::Permissions::from_mode(0o755)).unwrap();
    let file = format!("go{version}.linux-{arch}.tar.gz");
    run_ok(Command::new("tar").args(["-czf"]).arg(dir.join(&file)).arg("-C").arg(dir.join("src")).arg("go"));
    let sum = if tamper { "0".repeat(64) } else { sha256(&dir.join(&file)) };
    let size = fs::metadata(dir.join(&file)).unwrap().len();
    fs::write(
        dir.join("index.json"),
        format!(
            r#"[{{"version":"go{version}","stable":true,"files":[{{"filename":"{file}","os":"linux","arch":"{arch}","kind":"archive","sha256":"{sum}","size":{size}}}]}}]"#
        ),
    )
    .unwrap();
    Some(Release {
        envs: vec![
            ("FORGE_GO_INDEX".into(), format!("file://{}/index.json", dir.display())),
            ("FORGE_GO_DL".into(), format!("file://{}/", dir.display())),
        ],
        dir,
    })
}

fn go_project(f: &Fixture) -> String {
    fs::remove_file(f.repo.join("Makefile")).unwrap();
    fs::write(f.repo.join("go.mod"), "module example.com/hello\n\ngo 1.99.0\n").unwrap();
    fs::write(f.repo.join("main.go"), "package main\nfunc main() {}\n").unwrap();
    commit_all(&f.repo, "go project");
    format!("file://{}", f.repo.display())
}

fn with<'a>(r: &'a Release) -> Vec<(&'a str, &'a str)> {
    r.envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

#[test]
fn a_missing_go_is_downloaded_verified_and_used_for_the_build() {
    let f = fixture(MAKEFILE);
    let Some(rel) = fake_go_release(f.repo.parent().unwrap(), "1.99.1", false) else { return };
    let url = go_project(&f);
    f.ok(&["list"]);
    let env = with(&rel);

    let o = f.forge_with(&["build", &url, "--name", "hello"], &env);
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(o.status.success(), "{out}{}", String::from_utf8_lossy(&o.stderr));
    // The plan says what will be downloaded and why; the download then happens.
    assert!(out.contains("toolchain") && out.contains("go 1.99.1") && out.contains("downloads"), "{out}");
    assert!(out.contains("Toolchains") && out.contains("download go 1.99.1"), "{out}");

    // The build used Forge's Go (and told it never to fetch another one).
    let run = Command::new(f.linkbin().join("hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "built-with-managed-go toolchain=local");
    assert!(f.root.join("toolchains/go-1.99.1/bin/go").is_file());
    assert!(f.root.join("toolchains/go-1.99.1/.forge-toolchain.json").is_file());
    assert!(f.ok(&["info", "hello"]).contains("toolchain:go-1.99.1"));
    let list = f.ok(&["toolchain", "list"]);
    assert!(list.contains("go-1.99.1"), "{list}");

    // The next build reuses it without any download, even with the "server" gone.
    f.ok(&["remove", "hello", "--yes"]);
    fs::remove_dir_all(&rel.dir).unwrap();
    let o = f.forge_with(&["build", &url, "--name", "hello"], &env);
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(o.status.success(), "{out}{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("already downloaded") && !out.contains("Toolchains"), "{out}");
    f.ok(&["remove", "hello", "--yes"]);

    // The toolchain is a cache Forge owns: it can be deleted and nothing else is affected.
    assert!(f.fails(&["toolchain", "remove", "go-9.9.9"]).contains("no toolchain named"));
    assert!(f.fails(&["toolchain", "remove", "../etc"]).contains("no toolchain named"));
    f.ok(&["toolchain", "remove", "go-1.99.1"]);
    assert!(!f.root.join("toolchains/go-1.99.1").exists());
    assert!(installed_state(&f.root).values().all(|v| v == "dir"));
}

#[test]
fn a_download_with_the_wrong_checksum_is_refused() {
    let f = fixture(MAKEFILE);
    let Some(rel) = fake_go_release(f.repo.parent().unwrap(), "1.99.1", true) else { return };
    let url = go_project(&f);
    f.ok(&["list"]);
    let o = f.forge_with(&["build", &url, "--name", "hello"], &with(&rel));
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("checksum mismatch") && err.contains("Not installing it"), "{err}");
    assert!(!f.root.join("toolchains/go-1.99.1").exists(), "an unverified toolchain must not be kept");
    let leftovers: Vec<_> = fs::read_dir(f.root.join("toolchains")).map(|d| d.flatten().collect()).unwrap_or_default();
    assert!(leftovers.is_empty(), "no partial downloads left behind: {leftovers:?}");
    assert!(f.ok(&["list"]).contains("no packages"));
}

#[test]
fn host_only_policy_never_downloads() {
    let f = fixture(MAKEFILE);
    let Some(rel) = fake_go_release(f.repo.parent().unwrap(), "1.99.1", false) else { return };
    let url = go_project(&f);
    f.ok(&["list"]);
    let mut env = with(&rel);
    env.push(("FORGE_TOOLCHAINS", "host"));
    let o = f.forge_with(&["build", &url, "--name", "hello"], &env);
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("needs go") && err.contains("FORGE_TOOLCHAINS=host"), "{err}");
    assert!(!f.root.join("toolchains").exists() || fs::read_dir(f.root.join("toolchains")).unwrap().next().is_none());
}

#[test]
fn toolchain_install_prefetches_and_unknown_tools_are_rejected() {
    let f = fixture(MAKEFILE);
    let Some(rel) = fake_go_release(f.repo.parent().unwrap(), "1.99.1", false) else { return };
    f.ok(&["list"]);
    let env = with(&rel);
    let o = f.forge_with(&["toolchain", "install", "go", "1.99"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(f.root.join("toolchains/go-1.99.1/bin/go").is_file());
    let o = f.forge_with(&["toolchain", "install", "go"], &env);
    assert!(String::from_utf8_lossy(&o.stdout).contains("already downloaded"));
    let o = f.forge_with(&["toolchain", "install", "go", "2.0"], &env);
    assert!(!o.status.success() || String::from_utf8_lossy(&o.stdout).contains("already"), "a newer one than exists can't be found");
    assert!(f.fails(&["toolchain", "install", "fortran"]).contains("no toolchain called"));
}

#[test]
fn rust_toolchains_are_assembled_from_their_components() {
    let f = fixture(MAKEFILE);
    let target = match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        _ => return,
    };
    let dist = f.repo.parent().unwrap().join("fake-rust-dist");
    let ver = "1.99.1";
    let mut manifest = format!("[pkg.rust]\nversion = \"{ver} (abc 2026-10-13)\"\n");
    for (comp, part, file, body) in [
        ("rustc", "rustc", "bin/rustc", "#!/bin/sh\necho rustc 1.99.1\n"),
        ("cargo", "cargo", "bin/cargo", "#!/bin/sh\ncase \"$1\" in fetch) exit 0;; install) root=\"\"; while [ $# -gt 0 ]; do [ \"$1\" = --root ] && root=$2; shift; done; mkdir -p \"$root/bin\"; printf '#!/bin/sh\\necho built-with-managed-rust\\n' > \"$root/bin/hello\"; chmod +x \"$root/bin/hello\";; esac\n"),
        ("rust-std", &format!("rust-std-{target}"), "lib/rustlib/std.txt", "std"),
    ] {
        let top = format!("{comp}-{ver}-{target}");
        let at = dist.join("src").join(&top).join(part).join(file);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(&at, body).unwrap();
        if file.starts_with("bin/") {
            fs::set_permissions(&at, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(dist.join("src").join(&top).join("components"), format!("{part}\n")).unwrap();
        let tarball = dist.join(format!("{top}.tar.xz"));
        run_ok(Command::new("tar").args(["-cJf"]).arg(&tarball).arg("-C").arg(dist.join("src")).arg(&top));
        manifest += &format!(
            "[pkg.{comp}.target.{target}]\navailable = true\nxz_url = \"file://{}\"\nxz_hash = \"{}\"\n",
            tarball.display(),
            sha256(&tarball)
        );
    }
    fs::write(dist.join("channel-rust-stable.toml"), manifest).unwrap();

    // A Rust project; `managed` makes Forge use its own compiler whatever the machine has.
    fs::remove_file(f.repo.join("Makefile")).unwrap();
    fs::write(f.repo.join("Cargo.toml"), "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\nrust-version = \"1.90\"\n").unwrap();
    fs::create_dir_all(f.repo.join("src")).unwrap();
    fs::write(f.repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    commit_all(&f.repo, "rust project");
    f.ok(&["list"]);
    let dist_url = format!("file://{}", dist.display());
    let env = [("FORGE_RUST_DIST", dist_url.as_str()), ("FORGE_TOOLCHAINS", "managed")];
    let o = f.forge_with(&["build", &format!("file://{}", f.repo.display()), "--name", "hello"], &env);
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(o.status.success(), "{out}{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("rust 1.99.1") && out.contains("FORGE_TOOLCHAINS=managed"), "{out}");
    for f_ in ["bin/rustc", "bin/cargo", "lib/rustlib/std.txt", ".forge-toolchain.json"] {
        assert!(f.root.join("toolchains/rust-1.99.1").join(f_).exists(), "{f_} missing from the assembled toolchain");
    }
    let run = Command::new(f.linkbin().join("hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "built-with-managed-rust");
}

#[test]
fn a_missing_meson_is_downloaded_from_a_pypi_style_index_and_used() {
    let f = fixture(MAKEFILE);
    let dir = f.repo.parent().unwrap().join("fake-pypi");
    let top = dir.join("src/meson-1.99.0");
    fs::create_dir_all(&top).unwrap();
    fs::write(
        top.join("meson.py"),
        r##"#!/usr/bin/env python3
import os, sys
a = sys.argv[1:]
if a[0] == "--version":
    print("1.99.0")
elif a[0] == "setup":
    os.makedirs(a[1], exist_ok=True)
    open(os.path.join(a[1], "prefix"), "w").write(a[a.index("--prefix") + 1])
elif a[0] == "compile":
    pass
elif a[0] == "install":
    obj = a[a.index("-C") + 1]
    dest = os.environ["DESTDIR"] + open(os.path.join(obj, "prefix")).read() + "/bin"
    os.makedirs(dest, exist_ok=True)
    p = os.path.join(dest, "hello")
    open(p, "w").write("#!/bin/sh\necho built-with-managed-meson\n")
    os.chmod(p, 0o755)
"##,
    )
    .unwrap();
    let tarball = dir.join("meson-1.99.0.tar.gz");
    run_ok(Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(dir.join("src")).arg("meson-1.99.0"));
    fs::write(
        dir.join("index.json"),
        format!(
            r#"{{"info":{{"version":"1.99.0"}},"urls":[{{"packagetype":"sdist","filename":"meson-1.99.0.tar.gz","url":"file://{}","size":{},"digests":{{"sha256":"{}"}}}}]}}"#,
            tarball.display(),
            fs::metadata(&tarball).unwrap().len(),
            sha256(&tarball)
        ),
    )
    .unwrap();

    fs::remove_file(f.repo.join("Makefile")).unwrap();
    fs::write(f.repo.join("meson.build"), "project('hello', 'c', meson_version: '>=1.99')\n").unwrap();
    commit_all(&f.repo, "meson project");
    f.ok(&["list"]);

    let index = format!("file://{}/index.json", dir.display());
    let o = f.forge_with(&["build", &format!("file://{}", f.repo.display()), "--name", "hello"], &[("FORGE_MESON_INDEX", index.as_str())]);
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(o.status.success(), "{out}{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("meson 1.99.0") && out.contains("downloads"), "{out}");
    assert!(f.root.join("toolchains/meson-1.99.0/bin/meson").is_file());
    let run = Command::new(f.linkbin().join("hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "built-with-managed-meson");
    assert!(f.ok(&["toolchain", "list"]).contains("meson-1.99.0"));
}

#[test]
fn meson_builds_use_a_python_that_forge_provides() {
    let f = fixture(MAKEFILE);
    let triple = match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        _ => return,
    };
    let dir = f.repo.parent().unwrap().join("fake-python-meson");
    fs::create_dir_all(&dir).unwrap();

    // A fake python-build-standalone release: bin/python3 just records that it was the one used.
    let py_bin = dir.join("pysrc/python/bin");
    fs::create_dir_all(&py_bin).unwrap();
    fs::write(py_bin.join("python3"), "#!/bin/sh\necho forge-python >> \"$FORGE_OBJ/python-used\" 2>/dev/null\nexec /usr/bin/python3 \"$@\"\n").unwrap();
    fs::set_permissions(py_bin.join("python3"), fs::Permissions::from_mode(0o755)).unwrap();
    let py_name = format!("cpython-3.99.0+20261026-{triple}-install_only.tar.gz");
    let py_tar = dir.join(&py_name);
    run_ok(Command::new("tar").arg("-czf").arg(&py_tar).arg("-C").arg(dir.join("pysrc")).arg("python"));
    fs::write(
        dir.join("python-release.json"),
        format!(
            r#"{{"assets":[{{"name":"{py_name}","size":1,"browser_download_url":"file://{}","digest":"sha256:{}"}}]}}"#,
            py_tar.display(),
            sha256(&py_tar)
        ),
    )
    .unwrap();

    // A fake PyPI for meson: meson.py is a tiny script run by whatever `python3` is first on PATH.
    let top = dir.join("mesonsrc/meson-1.99.0");
    fs::create_dir_all(&top).unwrap();
    fs::write(
        top.join("meson.py"),
        r##"#!/usr/bin/env python3
import os, sys
a = sys.argv[1:]
if a[0] == "setup":
    os.makedirs(a[1], exist_ok=True)
    open(os.path.join(a[1], "prefix"), "w").write(a[a.index("--prefix") + 1])
elif a[0] == "install":
    obj = a[a.index("-C") + 1]
    dest = os.environ["DESTDIR"] + open(os.path.join(obj, "prefix")).read() + "/bin"
    os.makedirs(dest, exist_ok=True)
    seen = "python-used" in os.listdir(obj)
    p = os.path.join(dest, "hello")
    open(p, "w").write("#!/bin/sh\necho forge-python-used=%s\n" % seen)
    os.chmod(p, 0o755)
"##,
    )
    .unwrap();
    let m_tar = dir.join("meson-1.99.0.tar.gz");
    run_ok(Command::new("tar").arg("-czf").arg(&m_tar).arg("-C").arg(dir.join("mesonsrc")).arg("meson-1.99.0"));
    fs::write(
        dir.join("meson-index.json"),
        format!(
            r#"{{"info":{{"version":"1.99.0"}},"urls":[{{"packagetype":"sdist","filename":"meson-1.99.0.tar.gz","url":"file://{}","size":1,"digests":{{"sha256":"{}"}}}}]}}"#,
            m_tar.display(),
            sha256(&m_tar)
        ),
    )
    .unwrap();

    fs::remove_file(f.repo.join("Makefile")).unwrap();
    fs::write(f.repo.join("meson.build"), "project('hello', 'c')\n").unwrap();
    commit_all(&f.repo, "meson project");
    f.ok(&["list"]);

    let (pi, mi) = (format!("file://{}/python-release.json", dir.display()), format!("file://{}/meson-index.json", dir.display()));
    let env = [("FORGE_PYTHON_INDEX", pi.as_str()), ("FORGE_MESON_INDEX", mi.as_str()), ("FORGE_TOOLCHAINS", "managed")];
    let o = f.forge_with(&["build", &format!("file://{}", f.repo.display()), "--name", "hello"], &env);
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(o.status.success(), "{out}{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("python 3.99.0") && out.contains("meson 1.99.0"), "both toolchains are planned:\n{out}");
    assert!(f.root.join("toolchains/python-3.99.0/bin/python3").is_file());
    assert!(f.root.join("toolchains/meson-1.99.0/bin/meson").is_file());
    let run = Command::new(f.linkbin().join("hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "forge-python-used=True", "meson ran under Forge's python");
    let list = f.ok(&["toolchain", "list"]);
    assert!(list.contains("python-3.99.0") && list.contains("meson-1.99.0"), "{list}");
}

// ---- install by name, build by link, archive sources, the default repository ----

const GREET_INSTALL: &str = "set -e\nD=\"$FORGE_STAGE$FORGE_PREFIX\"\nmkdir -p \"$D/bin\"\n{ echo '#!/bin/sh'; cat greet.sh; } > \"$D/bin/greet\"\nchmod +x \"$D/bin/greet\"\n";
const GREET_PATCH: &str = "--- a/greet.sh\n+++ b/greet.sh\n@@ -1 +1 @@\n-echo plain\n+echo patched\n";

/// A repository (used as FORGE_REPO) holding an archive recipe with a patch, plus a
/// same-named recipe under foundry/ that must lose against recipes/.
fn archive_repo(f: &Fixture) -> (PathBuf, Vec<(&'static str, String)>) {
    let parent = f.repo.parent().unwrap().to_path_buf();
    let src = parent.join("greet-1.0");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("greet.sh"), "echo plain\n").unwrap();
    fs::write(src.join("install.sh"), GREET_INSTALL).unwrap();
    let tarball = parent.join("greet-1.0.tar.gz");
    assert!(Command::new("tar").args(["-czf"]).arg(&tarball).args(["-C"]).arg(&parent).arg("greet-1.0").status().unwrap().success());
    let sum = Command::new("sha256sum").arg(&tarball).output().unwrap();
    let sha = String::from_utf8_lossy(&sum.stdout).split_whitespace().next().unwrap().to_string();
    let recipe = format!(
        "[package]\nname = \"greet\"\nversion = \"1.0\"\ndescription = \"curated\"\narchive = \"file://{}\"\nsha256 = \"{sha}\"\nstrip_components = 1\npatches = [\"patches/fix.patch\"]\n[build]\nsteps = [\"sh install.sh\"]\n",
        tarball.display()
    );
    let void_recipe = recipe.replace("curated", "imported").replace("patches = [\"patches/fix.patch\"]\n", "");
    let repo = make_repo(
        &parent,
        "official",
        &[
            ("recipes/greet/recipe.toml", &recipe),
            ("recipes/greet/patches/fix.patch", GREET_PATCH),
            ("foundry/greet.toml", &void_recipe),
            ("foundry/other.toml", &void_recipe.replace("\"greet\"", "\"other\"")),
        ],
    );
    (repo, vec![("FORGE_ALLOW_FILE_SOURCES", "1".to_string())])
}

#[test]
fn install_takes_names_from_the_default_repository_and_applies_archive_patches() {
    let f = fixture("");
    if !f.sandbox {
        eprintln!("skipping: patches are applied inside the sandbox");
        return;
    }
    let (repo, envs) = archive_repo(&f);
    let url = repo.to_str().unwrap().to_string();
    let e: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).chain([("FORGE_REPO", url.as_str())]).collect();

    // First use sets the repository up by itself; recipes/ wins over foundry/.
    let o = f.forge_with(&["install", "greet"], &e);
    assert!(o.status.success(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert_eq!(run_bin(&f, "greet"), "patched");
    assert!(f.ok(&["verify"]).contains("intact"));

    // An imported recipe is installable too (after greet gives up bin/greet).
    f.ok(&["remove", "greet"]);
    let o = f.forge_with(&["install", "other"], &e);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let list = String::from_utf8_lossy(&f.forge_with(&["tap", "list"], &e).stdout).into_owned();
    assert!(list.contains("1 curated") && list.contains("2 imported"), "{list}");
    f.ok(&["remove", "other"]);
    assert!(!f.root.join("bin/greet").exists() && !f.root.join("bin/other").exists());
}

#[test]
fn install_only_takes_names_and_points_links_to_build() {
    let f = fixture(MAKEFILE);
    let err = f.fails(&["install", "https://github.com/user/project"]);
    assert!(err.contains("forge build"), "{err}");
    let err = f.fails(&["install", "no-such-package"]);
    assert!(err.contains("no-such-package"), "{err}");
    // `forge build` still takes a link (here a local repository).
    let out = f.ok(&["build", f.repo_str(), "--name", "hello"]);
    assert!(out.contains("hello"), "{out}");
    assert_eq!(run_bin(&f, "hello"), "hello-v1");
}

#[test]
fn sync_refreshes_the_repository_and_reports_updates_for_installed_packages() {
    let f = fixture("");
    if !f.sandbox {
        eprintln!("skipping: needs the sandbox");
        return;
    }
    let (repo, envs) = archive_repo(&f);
    let url = repo.to_str().unwrap().to_string();
    let e: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).chain([("FORGE_REPO", url.as_str())]).collect();
    let run = |args: &[&str]| {
        let o = f.forge_with(args, &e);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    run(&["install", "greet"]);
    let out = run(&["sync"]);
    assert!(out.contains("up to date") && !out.contains("update(s) available"), "{out}");

    // The repository publishes a new version of greet.
    let file = repo.join("recipes/greet/recipe.toml");
    let text = fs::read_to_string(&file).unwrap();
    let old_sha = text.lines().find(|l| l.starts_with("sha256")).unwrap().to_string();
    let new = text.replace("version = \"1.0\"", "version = \"1.1\"").replace(&old_sha, &format!("sha256 = \"{}\"", "ab".repeat(32)));
    fs::write(&file, new).unwrap();
    commit_all(&repo, "greet 1.1");

    let out = run(&["sync"]);
    assert!(out.contains("1 update(s) available") && out.contains("greet") && out.contains("1.1"), "{out}");
    // Sync only checks: the installed version is untouched.
    assert_eq!(run_bin(&f, "greet"), "patched");
    assert!(run(&["list"]).contains("1.0"));
}
