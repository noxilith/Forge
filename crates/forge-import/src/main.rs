use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Generates Forge recipes from other distributions' package definitions.
#[derive(Parser)]
#[command(name = "forge-import", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert a void-packages checkout (templates are parsed, never run).
    Void {
        /// The void-packages directory (needs srcpkgs/ and common/environment/).
        dir: PathBuf,
        /// Where to write the recipes (replaced if it exists).
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Void { dir, out } => {
            if !dir.join("srcpkgs").is_dir() {
                bail!("{} has no srcpkgs/ directory; is it a void-packages checkout?", dir.display());
            }
            let report = forge_import::convert::run(&dir, &out)?;
            std::fs::write(out.join("REPORT.md"), report.to_markdown())?;
            std::fs::write(out.join("IMPORTED_FROM"), format!("void-packages {}\n", report.commit))?;
            if let Ok(l) = std::fs::read_to_string(dir.join("COPYING")) {
                std::fs::write(out.join("LICENSE-void-packages"), l)?;
            }
            println!("{} templates, {} recipes written to {}", report.total, report.imported, out.display());
            Ok(())
        }
    }
}
