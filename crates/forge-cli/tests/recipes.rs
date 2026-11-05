//! The recipes in the `tap/` repository must stay valid.

use forge_core::Recipe;

#[test]
fn shipped_recipes_parse_and_are_consistent() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tap/recipes");
    // The recipes live in their own repository (Forge-Packages), cloned here as `tap/`.
    // A fresh clone of Forge alone doesn't have it, and there is nothing to check then.
    if !dir.is_dir() {
        eprintln!("skipping: tap/recipes not present (clone Forge-Packages into tap/ to check the recipes)");
        return;
    }
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "toml") {
            let r = Recipe::from_file(&p).unwrap_or_else(|e| panic!("{}: {e:#}", p.display()));
            let stem = p.file_stem().unwrap().to_string_lossy().to_string();
            assert_eq!(r.package.name.as_deref(), Some(stem.as_str()), "{}", p.display());
            assert!(r.package.repo.as_deref().is_some_and(|u| u.starts_with("https://")), "{}", p.display());
            n += 1;
        }
    }
    assert!(n >= 8, "expected the tap's recipes, found {n}");
    // Small self-contained projects are deliberately not in the tap.
    assert!(!dir.join("fastfetch.toml").exists());

    // The compositors keep their config inside the package without exporting
    // XDG variables that the programs they start would inherit.
    let niri = Recipe::from_file(&dir.join("niri.toml")).unwrap();
    assert_eq!(niri.run.isolate.as_deref(), Some("none"));
    assert!(niri.run.env.contains_key("NIRI_CONFIG"));
    assert!(niri.run.seed.contains_key("config/niri/config.kdl"));
    let hypr = Recipe::from_file(&dir.join("hyprland.toml")).unwrap();
    assert_eq!(hypr.run.isolate.as_deref(), Some("none"));
    assert_eq!(hypr.run.args_commands, vec!["Hyprland"]);
}
