//! One backend per build system. A backend only *describes* shell commands;
//! running them is the sandbox's job. Commands refer to these variables:
//! `$FORGE_SRC $FORGE_OBJ $FORGE_PREFIX $FORGE_STAGE $FORGE_JOBS` (and
//! `$DESTDIR` during the install phase).

use forge_core::util::sh_quote;
use forge_core::{BuildSystem, Recipe};
use std::path::PathBuf;

pub struct Ctx<'a> {
    pub recipe: &'a Recipe,
    /// Where the project to build is, on the host (for looking at files).
    pub src_host: PathBuf,
    pub have_ninja: bool,
    /// Final package directory (the build's `--prefix`).
    pub prefix: PathBuf,
    /// Library directories installed binaries must be able to find at runtime:
    /// the package's own `lib` and those of its runtime Forge dependencies.
    pub rpath_dirs: Vec<String>,
}

#[derive(Default, Debug)]
pub struct Plan {
    /// Run with network access.
    pub fetch: Vec<String>,
    /// Run offline.
    pub build: Vec<String>,
    /// Run offline with DESTDIR set; must install under `$FORGE_STAGE$FORGE_PREFIX`.
    pub install: Vec<String>,
}

pub trait Backend {
    fn system(&self) -> BuildSystem;
    /// Host tools needed in addition to the recipe's own `tools`.
    fn tools(&self, ctx: &Ctx) -> Vec<String>;
    fn plan(&self, ctx: &Ctx) -> Plan;
}

/// Host tools a build system always needs, knowable without looking at the source.
pub fn base_tools(sys: BuildSystem) -> Vec<String> {
    let t: &[&str] = match sys {
        BuildSystem::Cargo => &["cargo"],
        BuildSystem::Cmake => &["cmake"],
        BuildSystem::Meson => &["meson", "ninja"],
        BuildSystem::Autotools | BuildSystem::Make => &["make"],
        BuildSystem::Go => &["go"],
        BuildSystem::Custom => &[],
    };
    t.iter().map(|s| s.to_string()).collect()
}

pub fn backend_for(sys: BuildSystem) -> Box<dyn Backend> {
    match sys {
        BuildSystem::Cargo => Box::new(Cargo),
        BuildSystem::Cmake => Box::new(Cmake),
        BuildSystem::Meson => Box::new(Meson),
        BuildSystem::Autotools => Box::new(Autotools),
        BuildSystem::Go => Box::new(Go),
        BuildSystem::Make => Box::new(Make),
        BuildSystem::Custom => Box::new(Custom),
    }
}

/// The full plan: backend defaults plus whatever the recipe adds.
pub fn plan_for(sys: BuildSystem, ctx: &Ctx) -> Plan {
    let mut p = backend_for(sys).plan(ctx);
    let mut fetch = ctx.recipe.build.fetch.clone();
    fetch.append(&mut p.fetch);
    p.fetch = fetch;
    p.install.extend(ctx.recipe.install.post.iter().cloned());
    p
}

fn args(ctx: &Ctx) -> String {
    ctx.recipe
        .build
        .configure_args
        .iter()
        .map(|a| format!(" {}", sh_quote(a)))
        .collect()
}

struct Cargo;
impl Backend for Cargo {
    fn system(&self) -> BuildSystem {
        BuildSystem::Cargo
    }
    fn tools(&self, _: &Ctx) -> Vec<String> {
        vec!["cargo".into()]
    }
    fn plan(&self, ctx: &Ctx) -> Plan {
        let locked = if ctx.src_host.join("Cargo.lock").exists() { " --locked" } else { "" };
        Plan {
            fetch: vec![format!("cargo fetch{locked}")],
            build: vec![],
            install: vec![format!(
                "cargo install --path . --root \"$FORGE_STAGE$FORGE_PREFIX\" --offline{locked} --no-track{}",
                args(ctx)
            )],
        }
    }
}

struct Cmake;
impl Backend for Cmake {
    fn system(&self) -> BuildSystem {
        BuildSystem::Cmake
    }
    fn tools(&self, ctx: &Ctx) -> Vec<String> {
        vec!["cmake".into(), if ctx.have_ninja { "ninja" } else { "make" }.into()]
    }
    fn plan(&self, ctx: &Ctx) -> Plan {
        let gen_ = if ctx.have_ninja { " -G Ninja" } else { "" };
        // CMake strips build-tree RPATHs on install, so say explicitly where
        // the libraries live. Forge packages are not in a system library path.
        let rpath = sh_quote(&format!("-DCMAKE_INSTALL_RPATH={}", ctx.rpath_dirs.join(";")));
        Plan {
            fetch: vec![],
            build: vec![
                format!(
                    "cmake -S . -B \"$FORGE_OBJ\"{gen_} -DCMAKE_BUILD_TYPE=Release \
                     -DCMAKE_INSTALL_PREFIX=\"$FORGE_PREFIX\" -DCMAKE_INSTALL_LIBDIR=lib \
                     {rpath} -DCMAKE_INSTALL_RPATH_USE_LINK_PATH=ON{}",
                    args(ctx)
                ),
                "cmake --build \"$FORGE_OBJ\" -j \"$FORGE_JOBS\"".into(),
            ],
            install: vec!["cmake --install \"$FORGE_OBJ\"".into()],
        }
    }
}

struct Meson;
impl Backend for Meson {
    fn system(&self) -> BuildSystem {
        BuildSystem::Meson
    }
    fn tools(&self, _: &Ctx) -> Vec<String> {
        vec!["meson".into(), "ninja".into()]
    }
    fn plan(&self, ctx: &Ctx) -> Plan {
        Plan {
            fetch: vec!["[ ! -d subprojects ] || meson subprojects download || true".into()],
            build: vec![
                format!(
                    "meson setup \"$FORGE_OBJ\" . --prefix \"$FORGE_PREFIX\" --libdir lib --buildtype release{}",
                    args(ctx)
                ),
                "meson compile -C \"$FORGE_OBJ\" -j \"$FORGE_JOBS\"".into(),
            ],
            install: vec!["meson install -C \"$FORGE_OBJ\" --no-rebuild".into()],
        }
    }
}

struct Autotools;
impl Backend for Autotools {
    fn system(&self) -> BuildSystem {
        BuildSystem::Autotools
    }
    fn tools(&self, ctx: &Ctx) -> Vec<String> {
        let mut t = vec!["make".to_string()];
        if !ctx.src_host.join("configure").exists() {
            t.push("autoreconf".into());
        }
        t
    }
    fn plan(&self, ctx: &Ctx) -> Plan {
        let mut build = Vec::new();
        if !ctx.src_host.join("configure").exists() {
            build.push(if ctx.src_host.join("autogen.sh").exists() {
                "NOCONFIGURE=1 sh ./autogen.sh".to_string()
            } else {
                "autoreconf -fi".to_string()
            });
        }
        build.push(format!("./configure --prefix=\"$FORGE_PREFIX\"{}", args(ctx)));
        build.push("make -j\"$FORGE_JOBS\"".into());
        Plan { fetch: vec![], build, install: vec!["make install".into()] }
    }
}

struct Go;
impl Backend for Go {
    fn system(&self) -> BuildSystem {
        BuildSystem::Go
    }
    fn tools(&self, _: &Ctx) -> Vec<String> {
        vec!["go".into()]
    }
    fn plan(&self, ctx: &Ctx) -> Plan {
        Plan {
            fetch: vec!["go mod download".into()],
            build: vec![],
            install: vec![format!(
                "GOBIN=\"$FORGE_STAGE$FORGE_PREFIX/bin\" go install{} {}",
                args(ctx),
                if ctx.recipe.build.targets.is_empty() {
                    "./...".to_string()
                } else {
                    ctx.recipe.build.targets.iter().map(|t| sh_quote(t)).collect::<Vec<_>>().join(" ")
                }
            )],
        }
    }
}

struct Make;
impl Backend for Make {
    fn system(&self) -> BuildSystem {
        BuildSystem::Make
    }
    fn tools(&self, _: &Ctx) -> Vec<String> {
        vec!["make".into()]
    }
    fn plan(&self, ctx: &Ctx) -> Plan {
        let vars = "PREFIX=\"$FORGE_PREFIX\" prefix=\"$FORGE_PREFIX\"";
        Plan {
            fetch: vec![],
            build: vec![format!("make -j\"$FORGE_JOBS\" {vars}{}", args(ctx))],
            install: vec![format!("make install {vars}{}", args(ctx))],
        }
    }
}

struct Custom;
impl Backend for Custom {
    fn system(&self) -> BuildSystem {
        BuildSystem::Custom
    }
    fn tools(&self, _: &Ctx) -> Vec<String> {
        vec![]
    }
    fn plan(&self, ctx: &Ctx) -> Plan {
        Plan { fetch: vec![], build: ctx.recipe.build.steps.clone(), install: vec![] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmake_plan_uses_prefix_and_destdir_install() {
        let r = Recipe::parse("[build]\nconfigure_args = [\"-DX=a b\"]").unwrap();
        let ctx = Ctx {
            recipe: &r,
            src_host: PathBuf::from("/nonexistent"),
            have_ninja: true,
            prefix: PathBuf::from("/forge/store/x/1"),
            rpath_dirs: vec!["/forge/store/x/1/lib".into(), "/forge/store/dep/1/lib".into()],
        };
        let p = plan_for(BuildSystem::Cmake, &ctx);
        assert!(p.build[0].contains("-G Ninja"));
        assert!(p.build[0].contains("CMAKE_INSTALL_PREFIX=\"$FORGE_PREFIX\""));
        assert!(p.build[0].contains("'-DX=a b'"));
        assert!(p.build[0].contains("'-DCMAKE_INSTALL_RPATH=/forge/store/x/1/lib;/forge/store/dep/1/lib'"));
        assert_eq!(p.install, vec!["cmake --install \"$FORGE_OBJ\""]);
    }
}
