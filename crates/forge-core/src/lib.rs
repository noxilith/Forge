//! Shared types for Forge: paths, manifests, recipes, build-system detection.

pub mod buildsys;
pub mod desktop;
pub mod manifest;
pub mod paths;
pub mod recipe;
pub mod ui;
pub mod util;

pub use buildsys::BuildSystem;
pub use manifest::{DepRef, FileEntry, FileKind, Manifest};
pub use paths::Paths;
pub use recipe::{Isolation, Recipe, RunSection};
