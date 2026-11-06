//! `forge binary`: make the signed prebuilt packages that `forge install` downloads.

use crate::taps;
use anyhow::{Context, Result, bail};
use forge_build::binary::{self, Identity};
use forge_core::{Paths, ui};
use forge_store::Store;
use std::path::Path;

pub fn keygen(dir: &Path) -> Result<()> {
    let public = binary::keygen(dir)?;
    println!();
    println!("  {} Wrote {} (keep it secret) and {}", ui::check(), dir.join("forge-binary.key").display(), dir.join("forge-binary.pub").display());
    println!("  public key: {public}");
    println!("  {}", ui::dim("Machines trust it via FORGE_BINARY_PUBKEY, a *.pub file in /etc/forge/binary-keys/, or OFFICIAL_PUBKEY at build time."));
    Ok(())
}

/// Pack installed packages (`names`, or every installed package with `all`) into `out`.
pub fn pack(paths: &Paths, names: &[String], all: bool, out: &Path, secret: &Path) -> Result<()> {
    let store = Store::open(paths)?;
    let installed = store.list()?;
    let wanted: Vec<_> = if all {
        installed.iter().collect()
    } else {
        names
            .iter()
            .map(|n| installed.iter().find(|m| &m.name == n).with_context(|| format!("{n} is not installed")))
            .collect::<Result<_>>()?
    };
    if wanted.is_empty() {
        bail!("nothing to pack: name packages, or use --all");
    }
    println!();
    for m in wanted {
        // The recipe must still be the one the package was built from, patches included.
        let entry = taps::lookup(paths, &m.name)?.with_context(|| format!("{} is not in the package repository, so there is no recipe to pack it for", m.name))?;
        if m.recipe.as_deref() != Some(entry.recipe.to_toml()?.as_str()) {
            bail!("{} was built from a different recipe than the repository has now; reinstall it first", m.name);
        }
        let id = Identity {
            root: paths.root().to_path_buf(),
            name: m.name.clone(),
            version_id: m.version_id.clone(),
            recipe_toml: entry.recipe.to_toml()?,
            patches: entry.recipe.patch_files()?,
        };
        let meta = binary::pack(&id, &paths.package_dir(&m.name, &m.version_id), out, secret)?;
        println!("  {} {} {}  {}", ui::check(), ui::bold(&m.name), m.version_id, ui::dim(&format!("{}  {}", ui::human_bytes(meta.size), &meta.key[..12])));
    }
    println!("\n  {}", ui::dim(&format!("upload the files in {} to the binary server (FORGE_BINARY_URL)", out.display())));
    Ok(())
}
