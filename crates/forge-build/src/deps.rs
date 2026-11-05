//! Checks that the host has what a build needs, before spending time on it.

use forge_core::util::{found_in, path_dirs};
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub fn sandbox_path_dirs() -> Vec<PathBuf> {
    crate::sandbox::SANDBOX_PATH.split(':').map(PathBuf::from).collect()
}

/// Tools missing from the directories the build will actually see.
pub fn missing_tools(tools: &[String], sandboxed: bool) -> Vec<String> {
    let dirs = if sandboxed { sandbox_path_dirs() } else { path_dirs() };
    tools.iter().filter(|t| !found_in(&dirs, t)).cloned().collect()
}

pub fn missing_pkgconfig(reqs: &[String]) -> Vec<String> {
    reqs.iter()
        .filter(|r| {
            !Command::new("pkg-config")
                .arg("--exists")
                .arg(r)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}
