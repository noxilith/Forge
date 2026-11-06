//! Orchestrates one build: set up the job directory, run the fetch phase with
//! network, then everything else offline, all inside the sandbox.

use crate::backends::{self, Ctx};
use crate::sandbox::{self, Run};
use anyhow::{Context, Result};
use forge_core::{BuildSystem, Recipe};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An installed (or about-to-be-installed) Forge package a build depends on.
#[derive(Debug, Clone)]
pub struct DepMount {
    pub name: String,
    /// The path the package has (or will have) in the store.
    pub prefix: PathBuf,
    /// Where its files are right now: the same as `prefix` if installed, or
    /// inside another build's stage if it is being built in the same plan.
    pub host: PathBuf,
    /// Needed at runtime (linked into the result), not just while building.
    pub runtime: bool,
}

pub struct BuildConfig {
    /// `builds/<job>`; holds `src/`, `stage/`, scratch dirs and `build.log`.
    pub job_dir: PathBuf,
    /// The final package directory (the build's `--prefix`).
    pub prefix: PathBuf,
    pub system: BuildSystem,
    pub recipe: Recipe,
    pub sandbox: bool,
    pub verbose: bool,
    pub timeout: Duration,
    pub jobs: usize,
    pub source_date_epoch: Option<i64>,
    /// All Forge packages this build can see, direct and transitive.
    pub deps: Vec<DepMount>,
    /// Compilers Forge provided (see `toolchain`), mounted read-only and put first on PATH.
    pub toolchains: Vec<crate::toolchain::Ready>,
    /// Patch files applied (in order, `-p<patch_strip>`) to the unpacked source before anything else.
    pub patches: Vec<PathBuf>,
    pub patch_strip: u32,
}

/// One shell script that applies the patches, with `patch` or, if the machine has none, `git apply`.
fn patch_script(strip: u32, files: &[String]) -> String {
    files
        .iter()
        .map(|f| {
            let q = forge_core::util::sh_quote(f);
            format!("if command -v patch >/dev/null 2>&1; then patch -Np{strip} -i {q}; else git apply -p{strip} {q}; fi")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn lib_dirs(prefix: &Path) -> [String; 2] {
    [format!("{}/lib", prefix.display()), format!("{}/lib64", prefix.display())]
}

impl BuildConfig {
    pub fn src(&self) -> PathBuf {
        self.job_dir.join("src")
    }
    pub fn stage(&self) -> PathBuf {
        self.job_dir.join("stage")
    }
    pub fn log(&self) -> PathBuf {
        self.job_dir.join("build.log")
    }
}

/// Tools the build needs: the backend's plus the recipe's own.
pub fn required_tools(cfg: &BuildConfig) -> Vec<String> {
    let ctx = ctx_for(cfg);
    let mut t = backends::backend_for(cfg.system).tools(&ctx);
    t.extend(cfg.recipe.build.tools.iter().cloned());
    t.sort();
    t.dedup();
    t
}

fn ctx_for(cfg: &BuildConfig) -> Ctx<'_> {
    let dirs = if cfg.sandbox {
        crate::deps::sandbox_path_dirs()
    } else {
        forge_core::util::path_dirs()
    };
    let mut rpath_dirs = vec![format!("{}/lib", cfg.prefix.display())];
    for d in cfg.deps.iter().filter(|d| d.runtime) {
        rpath_dirs.extend(lib_dirs(&d.prefix));
    }
    Ctx {
        recipe: &cfg.recipe,
        src_host: workdir_host(cfg),
        have_ninja: forge_core::util::found_in(&dirs, "ninja"),
        prefix: cfg.prefix.clone(),
        rpath_dirs,
    }
}

fn workdir_host(cfg: &BuildConfig) -> PathBuf {
    match &cfg.recipe.build.subdir {
        Some(s) => cfg.src().join(s),
        None => cfg.src(),
    }
}

/// The commands that will run, per phase, for display before building.
pub fn describe(cfg: &BuildConfig) -> Vec<(&'static str, Vec<String>)> {
    let plan = backends::plan_for(cfg.system, &ctx_for(cfg));
    let mut out = Vec::new();
    if !cfg.patches.is_empty() {
        out.push(("patch", vec![format!("apply {} patch file(s) with -p{}", cfg.patches.len(), cfg.patch_strip)]));
    }
    out.extend([("fetch (network)", plan.fetch), ("build", plan.build), ("install", plan.install)]);
    out
}

/// Run the build. Returns the stage directory: the package is at
/// `stage/<prefix>`.
pub fn build(cfg: &BuildConfig) -> Result<PathBuf> {
    let job = &cfg.job_dir;
    let stage = cfg.stage();
    let scratch: Vec<(&str, PathBuf)> = ["obj", "home", "cargo-home", "gopath", "target", "tmp"]
        .iter()
        .map(|n| (*n, job.join(n)))
        .collect();
    fs::create_dir_all(&stage)?;
    for (_, p) in &scratch {
        fs::create_dir_all(p)?;
    }
    if cfg.sandbox {
        sandbox::check().map_err(anyhow::Error::msg)?;
    } else if cfg.deps.iter().any(|d| d.host != d.prefix) {
        anyhow::bail!(
            "building a package whose dependencies are not installed yet needs the sandbox \
             (install the dependencies first, or drop --insecure-no-sandbox)"
        );
    }
    // Where each patch file is, as the build sees it.
    let patch_inside: Vec<String> = cfg
        .patches
        .iter()
        .enumerate()
        .map(|(i, f)| if cfg.sandbox { format!("/build/patches/{:03}.patch", i + 1) } else { f.to_string_lossy().into_owned() })
        .collect();
    let ro_binds: Vec<(PathBuf, String)> = if cfg.sandbox {
        cfg.deps
            .iter()
            .map(|d| (d.host.clone(), d.prefix.to_string_lossy().into_owned()))
            .chain(cfg.toolchains.iter().map(|t| (t.dir.clone(), t.dir.to_string_lossy().into_owned())))
            .chain(cfg.patches.iter().cloned().zip(patch_inside.iter().cloned()))
            .collect()
    } else {
        Vec::new()
    };

    // Paths as the build sees them.
    let p = |inside: &str, host: PathBuf| -> String {
        if cfg.sandbox { inside.to_string() } else { host.to_string_lossy().into_owned() }
    };
    let src_in = p("/build/src", cfg.src());
    let stage_in = p("/build/stage", stage.clone());
    let obj_in = p("/build/obj", job.join("obj"));
    let home_in = p("/build/home", job.join("home"));
    let cwd = match &cfg.recipe.build.subdir {
        Some(s) => format!("{src_in}/{s}"),
        None => src_in.clone(),
    };
    let binds: Vec<(PathBuf, String)> = vec![
        (cfg.src(), "/build/src".into()),
        (stage.clone(), "/build/stage".into()),
        (job.join("obj"), "/build/obj".into()),
        (job.join("home"), "/build/home".into()),
        (job.join("cargo-home"), "/build/cargo-home".into()),
        (job.join("gopath"), "/build/gopath".into()),
        (job.join("target"), "/build/target".into()),
    ];

    let path_var = if cfg.sandbox {
        sandbox::SANDBOX_PATH.to_string()
    } else {
        std::env::var("PATH").unwrap_or_default()
    };
    let mut env: Vec<(String, String)> = vec![
        ("PATH".into(), path_var),
        ("HOME".into(), home_in),
        ("TMPDIR".into(), p("/tmp", job.join("tmp"))),
        ("LANG".into(), "C.UTF-8".into()),
        ("TERM".into(), "dumb".into()),
        ("FORGE_PREFIX".into(), cfg.prefix.to_string_lossy().into_owned()),
        ("FORGE_STAGE".into(), stage_in.clone()),
        ("FORGE_SRC".into(), src_in),
        ("FORGE_OBJ".into(), obj_in),
        ("FORGE_JOBS".into(), cfg.jobs.to_string()),
        ("CARGO_HOME".into(), p("/build/cargo-home", job.join("cargo-home"))),
        ("CARGO_TARGET_DIR".into(), p("/build/target", job.join("target"))),
        ("GOPATH".into(), p("/build/gopath", job.join("gopath"))),
        ("GOFLAGS".into(), "-modcacherw".into()),
    ];
    if let Some(t) = cfg.source_date_epoch {
        env.push(("SOURCE_DATE_EPOCH".into(), t.to_string()));
    }
    // The sandbox starts from an empty environment. Only toolchain
    // configuration the user set deliberately is passed through; nothing else.
    for (k, v) in std::env::vars() {
        if passthrough(&k) {
            env.retain(|(ek, _)| ek != &k);
            env.push((k, v));
        }
    }
    for (k, v) in &cfg.recipe.build.env {
        env.retain(|(ek, _)| ek != k);
        env.push((k.clone(), v.clone()));
    }

    apply_deps(cfg, &mut env);
    // Forge's own compilers win over whatever the machine has.
    for t in &cfg.toolchains {
        prepend_env(&mut env, "PATH", &t.bin_dir().to_string_lossy(), ":");
        for (k, v) in t.env() {
            env.retain(|(ek, _)| ek != &k);
            env.push((k, v));
        }
    }

    let plan = backends::plan_for(cfg.system, &ctx_for(cfg));
    let log = cfg.log();
    // Patches go first (they may change Cargo.toml and the like), offline, inside the sandbox.
    let patch_cmds: Vec<String> = if patch_inside.is_empty() { vec![] } else { vec![patch_script(cfg.patch_strip, &patch_inside)] };
    let phases: [(&str, &[String], bool, bool); 4] = [
        ("patch", &patch_cmds, false, false),
        ("fetch", &plan.fetch, true, false),
        ("build", &plan.build, false, false),
        ("install", &plan.install, false, true),
    ];
    for (phase, cmds, network, destdir) in phases {
        for script in cmds {
            let mut phase_env = env.clone();
            if destdir {
                phase_env.push(("DESTDIR".into(), stage_in.clone()));
            }
            if !network {
                phase_env.push(("GOPROXY".into(), "off".into()));
                phase_env.push(("CARGO_NET_OFFLINE".into(), "true".into()));
            }
            let first_line = script.lines().next().unwrap_or("");
            let label = step_label(cfg, phase, script);
            if cfg.verbose {
                println!("  {} {label}", forge_core::ui::arrow());
            }
            let spinner = forge_core::ui::Spinner::start(&label, !cfg.verbose);
            let result = sandbox::run(&Run {
                script,
                cwd: &cwd,
                env: &phase_env,
                binds: &binds,
                ro_binds: &ro_binds,
                network,
                log: &log,
                verbose: cfg.verbose,
                spinner: Some(&spinner),
                timeout: cfg.timeout,
                insecure: !cfg.sandbox,
            })
            .with_context(|| format!("{phase} step failed: {first_line}"));
            spinner.finish(result.is_ok(), "");
            result?;
        }
    }
    Ok(stage)
}

fn prepend_env(env: &mut Vec<(String, String)>, key: &str, value: &str, sep: &str) {
    if value.is_empty() {
        return;
    }
    match env.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) if !v.is_empty() => *v = format!("{value}{sep}{v}"),
        Some((_, v)) => *v = value.to_string(),
        None => env.push((key.to_string(), value.to_string())),
    }
}

/// Make the Forge dependencies usable: find tools, headers, pkg-config files
/// and libraries at build time, and make sure the result finds its libraries
/// at runtime without LD_LIBRARY_PATH.
fn apply_deps(cfg: &BuildConfig, env: &mut Vec<(String, String)>) {
    if cfg.deps.is_empty() {
        return;
    }
    let join = |v: Vec<String>| v.join(":");
    let prefixes: Vec<String> = cfg.deps.iter().map(|d| d.prefix.display().to_string()).collect();
    let each = |suffixes: &[&str]| -> Vec<String> {
        cfg.deps
            .iter()
            .flat_map(|d| suffixes.iter().map(move |s| format!("{}/{s}", d.prefix.display())))
            .collect()
    };
    env.push(("FORGE_DEPS".into(), prefixes.join(" ")));
    prepend_env(env, "PATH", &join(each(&["bin"])), ":");
    prepend_env(env, "PKG_CONFIG_PATH", &join(each(&["lib/pkgconfig", "lib64/pkgconfig", "share/pkgconfig"])), ":");
    prepend_env(env, "CMAKE_PREFIX_PATH", &join(prefixes), ":");
    prepend_env(env, "LD_LIBRARY_PATH", &join(each(&["lib", "lib64"])), ":");

    // Runtime search paths. CMake gets them through CMAKE_INSTALL_RPATH (it
    // would rewrite a linker-flag RPATH on install); everything else through
    // the linker flags.
    let mut runtime_dirs = vec![format!("{}/lib", cfg.prefix.display())];
    for d in cfg.deps.iter().filter(|d| d.runtime) {
        runtime_dirs.extend(lib_dirs(&d.prefix));
    }
    // Link time only: when a shared library of ours was itself linked against another one
    // (hyprtoolkit -> Abseil), the linker must be able to find that second one to resolve the
    // first's symbols. Unlike rpath this is not written into the result, so it is safe for
    // every build system, CMake included.
    let rpath_link: Vec<String> = cfg
        .deps
        .iter()
        .filter(|d| d.runtime)
        .flat_map(|d| lib_dirs(&d.prefix))
        .map(|d| format!("-Wl,-rpath-link,{d}"))
        .collect();
    match cfg.system {
        BuildSystem::Cmake => prepend_env(env, "LDFLAGS", &rpath_link.join(" "), " "),
        BuildSystem::Cargo => {
            let flags: Vec<String> = runtime_dirs
                .iter()
                .map(|d| format!("-C link-arg=-Wl,-rpath,{d}"))
                .chain(rpath_link.iter().map(|f| format!("-C link-arg={f}")))
                .collect();
            prepend_env(env, "RUSTFLAGS", &flags.join(" "), " ");
        }
        _ => {
            let mut flags: Vec<String> = Vec::new();
            for d in cfg.deps.iter().filter(|d| d.runtime) {
                flags.extend(lib_dirs(&d.prefix).map(|l| format!("-L{l}")));
            }
            flags.extend(runtime_dirs.iter().map(|d| format!("-Wl,-rpath,{d}")));
            flags.extend(rpath_link);
            prepend_env(env, "LDFLAGS", &flags.join(" "), " ");
        }
    }
}

/// Host environment variables that configure the compiler toolchain.
fn passthrough(name: &str) -> bool {
    matches!(
        name,
        "CC" | "CXX" | "AR" | "LD" | "CFLAGS" | "CXXFLAGS" | "CPPFLAGS" | "LDFLAGS" | "RUSTFLAGS"
            | "BINDGEN_EXTRA_CLANG_ARGS" | "LIBCLANG_PATH" | "PKG_CONFIG_PATH" | "PKG_CONFIG_LIBDIR"
    ) || (name.starts_with("CARGO_TARGET_") && name.ends_with("_LINKER"))
}

/// A human name for a build step: "configure", "compile", ... for the
/// standard commands, the (shortened) command itself for ones from a recipe.
fn step_label(cfg: &BuildConfig, phase: &str, script: &str) -> String {
    let own = |s: &str| format!("{} {}", forge_core::ui::dim("$"), forge_core::ui::truncate(s.lines().next().unwrap_or(""), 48));
    if cfg.recipe.build.steps.iter().chain(&cfg.recipe.build.fetch).chain(&cfg.recipe.install.post).any(|c| c == script) {
        return own(script);
    }
    let word = script.split_whitespace().next().unwrap_or("");
    match (phase, cfg.system) {
        ("patch", _) => "apply patches".into(),
        ("fetch", _) => "fetch dependencies".into(),
        ("build", BuildSystem::Cmake | BuildSystem::Meson) if word == "cmake" && script.contains(" -S ") || word == "meson" && script.contains(" setup ") => {
            "configure".into()
        }
        ("build", BuildSystem::Autotools) if script.contains("configure") && !script.starts_with("make") => "configure".into(),
        ("build", BuildSystem::Autotools) if word.contains("autoreconf") || script.contains("autogen") => "generate build files".into(),
        ("build", _) => "compile".into(),
        ("install", BuildSystem::Cargo | BuildSystem::Go) => "compile and install".into(),
        ("install", _) => "stage files".into(),
        _ => own(script),
    }
}

/// Last lines of the build log, for error reports.
pub fn log_tail(log: &Path, lines: usize) -> String {
    let text = fs::read_to_string(log).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
