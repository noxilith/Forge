mod install;
mod taps;
mod view;
mod integrate;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use forge_core::{BuildSystem, Paths};
use forge_store::Store;
use install::{Flags, InstallRequest};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "forge", version, about = "A source-based package manager with isolated, cleanly removable packages")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct BuildFlags {
    /// Don't ask for confirmation.
    #[arg(short, long)]
    yes: bool,
    /// Show build output live instead of logging it.
    #[arg(short, long)]
    verbose: bool,
    /// Build WITHOUT the bubblewrap sandbox (dangerous).
    #[arg(long)]
    insecure_no_sandbox: bool,
    /// Keep the build directory after a successful build.
    #[arg(long)]
    keep_build: bool,
    /// List every command that will run, not only the ones a recipe supplies.
    #[arg(long)]
    details: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build a package from source and install it.
    Install {
        /// Git URL, path to a local git repository, or a recipe name.
        target: String,
        /// Tag, branch or commit to build (default: the default branch).
        #[arg(long)]
        rev: Option<String>,
        /// Package name (default: from the recipe or the repository name).
        #[arg(long)]
        name: Option<String>,
        /// Use this recipe file.
        #[arg(long)]
        recipe: Option<PathBuf>,
        /// Force a build system instead of detecting it.
        #[arg(long)]
        build_system: Option<String>,
        /// Don't use a recipe from your recipes/taps even if one describes this repository.
        #[arg(long)]
        ignore_taps: bool,
        /// Where the program keeps its files: `xdg` (default; XDG dirs inside the package's data
        /// directory), `home` (also HOME), or `none` (leave the environment alone).
        #[arg(long, value_parser = ["xdg", "home", "none"])]
        isolate: Option<String>,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Remove a package, and the dependencies nothing else needs any more.
    Remove {
        package: String,
        /// Show what would be removed without doing it.
        #[arg(long)]
        dry_run: bool,
        /// Leave automatically installed dependencies in place.
        #[arg(long)]
        keep_deps: bool,
        /// Don't ask before removing session files from system directories.
        #[arg(short, long)]
        yes: bool,
        /// Keep the package's data directory (its configuration and other files).
        #[arg(long)]
        keep_data: bool,
    },
    /// Remove automatically installed dependencies that nothing needs.
    Autoremove {
        #[arg(long)]
        dry_run: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Register a package's session files (compositors, window managers) with the display manager. Needs sudo.
    Integrate {
        package: String,
        /// Show the files that would be installed and stop.
        #[arg(long)]
        dry_run: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove the system files `integrate` installed for a package.
    Unintegrate {
        package: String,
        #[arg(short, long)]
        yes: bool,
    },
    /// List installed packages.
    List,
    /// Show details of an installed package.
    Info { package: String },
    /// Rebuild a package from the latest commit of its tracked revision.
    Update {
        package: String,
        /// Use the current recipe from your recipes/taps instead of the one saved at install time.
        #[arg(long)]
        refresh_recipe: bool,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Update every installed package.
    Upgrade {
        #[arg(long)]
        refresh_recipe: bool,
        #[command(flatten)]
        flags: BuildFlags,
    },
    /// Manage recipe repositories (taps): git repos of `recipes/<name>.toml`.
    Tap {
        #[command(subcommand)]
        cmd: TapCmd,
    },
    /// Delete build directories, trash and (with --all) cached sources.
    Clean {
        #[arg(long)]
        all: bool,
    },
    /// Search local recipes.
    Search { query: String },
    /// Link installed commands into a directory already on PATH (default ~/.local/bin).
    /// Repairs packages installed before that was automatic.
    Relink { package: Option<String> },
    /// Check installed files against their manifests.
    Verify { package: Option<String> },
    /// Compilers (Go, Rust) that Forge downloads and keeps for builds that need them.
    Toolchain {
        #[command(subcommand)]
        cmd: ToolchainCmd,
    },
    /// Check that this machine can run Forge builds.
    Doctor,
    /// Print the shell line that puts Forge's commands on PATH.
    Env,
}

#[derive(Subcommand)]
enum ToolchainCmd {
    /// Show the toolchains Forge keeps and what the machine itself has.
    List,
    /// Download one now (e.g. for offline use). Builds normally do this by themselves.
    Install {
        /// go | rust
        tool: String,
        /// At least this version (default: the newest stable release).
        version: Option<String>,
    },
    /// Delete a downloaded toolchain, e.g. `go-1.27.3`. Builds download it again if needed.
    Remove { name: String },
}

#[derive(Subcommand)]
enum TapCmd {
    /// Add a tap from a git URL or a local git repository.
    Add {
        source: String,
        /// Name for the tap (default: the repository name).
        #[arg(long)]
        name: Option<String>,
    },
    /// List taps and their recipes.
    List,
    /// Remove a tap. Packages installed from it stay installed.
    Remove { name: String },
    /// Fetch the latest recipes (all taps, or one).
    Update { name: Option<String> },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("{} {e:#}", forge_core::ui::red("error:"));
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    // Never build or install as root: untrusted code, and Forge needs no privileges.
    if unsafe { libc::geteuid() } == 0 && !matches!(cli.cmd, Cmd::Doctor | Cmd::Env) {
        bail!("refusing to run as root: Forge builds untrusted code and needs no privileges");
    }
    let paths = Paths::from_env()?;
    match cli.cmd {
        Cmd::Install { target, rev, name, recipe, build_system, ignore_taps, isolate, flags } => {
            let (repo, recipe, origin, name) =
                install::resolve_target(&paths, &target, recipe.as_ref(), name.as_deref(), ignore_taps)?;
            let rev_spec = rev.or_else(|| recipe.as_ref().and_then(|r| r.package.rev.clone()));
            install::install(
                &paths,
                InstallRequest {
                    repo,
                    name,
                    rev_spec,
                    recipe,
                    recipe_origin: origin,
                    system_override: build_system.as_deref().map(BuildSystem::parse).transpose()?,
                    isolate,
                },
                &flags.into(),
            )
        }
        Cmd::Update { package, refresh_recipe, flags } => {
            install::update(&paths, vec![package], &flags.into(), refresh_recipe)
        }
        Cmd::Upgrade { refresh_recipe, flags } => install::update(&paths, vec![], &flags.into(), refresh_recipe),
        Cmd::Tap { cmd } => match cmd {
            TapCmd::Add { source, name } => taps::add(&paths, &source, name.as_deref()),
            TapCmd::Remove { name } => taps::remove(&paths, &name),
            TapCmd::Update { name } => taps::update(&paths, name.as_deref()),
            TapCmd::List => view::tap_list(&paths),
        },
        Cmd::Integrate { package, dry_run, yes } => integrate::integrate(&paths, &package, dry_run, yes),
        Cmd::Unintegrate { package, yes } => integrate::unintegrate(&paths, &package, yes),
        Cmd::Remove { package, dry_run, keep_deps, yes, keep_data } => {
            let mut store = Store::open(&paths)?;
            if store.get(&package)?.is_none() {
                bail!("package '{package}' is not installed");
            }
            let mut names = vec![package.clone()];
            if !keep_deps {
                names.extend(store.orphans_if_removed(&[package.clone()])?);
            }
            let title = if names.len() > 1 { format!("Removing {} and what only it needed", names[0]) } else { format!("Removing {}", names[0]) };
            view::remove_all(&paths, &mut store, &title, &names, dry_run, yes, keep_data)
        }
        Cmd::Autoremove { dry_run, yes } => {
            let mut store = Store::open(&paths)?;
            let names = store.orphans_if_removed(&[])?;
            if names.is_empty() {
                println!("\n  {} no unneeded dependencies", forge_core::ui::check());
                return Ok(());
            }
            view::remove_all(&paths, &mut store, "Removing unneeded dependencies", &names, dry_run, yes, false)
        }
        Cmd::List => view::list(&Store::open(&paths)?),
        Cmd::Info { package } => view::info(&paths, &Store::open(&paths)?, &package),
        Cmd::Clean { all } => view::clean(&paths, all),
        Cmd::Search { query } => view::search(&paths, &query),
        Cmd::Verify { package } => view::verify(&Store::open(&paths)?, package.as_deref()),
        Cmd::Relink { package } => view::relink(&paths, package),
        Cmd::Toolchain { cmd } => match cmd {
            ToolchainCmd::List => view::toolchain_list(&paths),
            ToolchainCmd::Install { tool, version } => view::toolchain_install(&paths, &tool, version.as_deref()),
            ToolchainCmd::Remove { name } => view::toolchain_remove(&paths, &name),
        },
        Cmd::Doctor => view::doctor(&paths),
        Cmd::Env => {
            println!("export PATH=\"{}:$PATH\"", paths.bin().display());
            Ok(())
        }
    }
}

impl From<BuildFlags> for Flags {
    fn from(f: BuildFlags) -> Self {
        Flags {
            yes: f.yes,
            verbose: f.verbose,
            sandbox: !f.insecure_no_sandbox,
            keep_build: f.keep_build,
            details: f.details,
        }
    }
}
