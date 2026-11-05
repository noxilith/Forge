//! Rewriting a package's display-manager session file (`.desktop`) so it can
//! be installed system-wide while the program itself stays in the Forge store.

use anyhow::{Result, bail};

/// Characters allowed in paths we embed in `.desktop` files and wrapper
/// scripts. Anything needing quoting is refused rather than escaped.
pub fn safe_path_chars(p: &str) -> bool {
    !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '+' | '-'))
}

pub struct Rewritten {
    pub content: String,
    /// The program the session runs, as resolved by the caller.
    pub target: String,
}

/// Point `Exec`/`TryExec` at `wrapper`, and mark the entry as Forge's in the
/// session list (`Name=... (Forge)`) so it can't be confused with a
/// distribution-provided entry of the same name.
///
/// `resolve` maps the original command (first word of `Exec=`) to the absolute
/// path the wrapper should run; it must fail for commands the package doesn't provide.
pub fn rewrite_session(
    content: &str,
    wrapper: &str,
    resolve: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<Rewritten> {
    if !safe_path_chars(wrapper) {
        bail!("path {wrapper} contains characters that cannot be used safely in a session file");
    }
    let mut out = String::new();
    let mut in_entry = false;
    let mut seen_entry = false;
    let mut target = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_entry = trimmed == "[Desktop Entry]";
            seen_entry |= in_entry;
            out.push_str(line);
        } else if let Some(rest) = line.strip_prefix("Exec=") {
            if !in_entry {
                bail!("unsupported session file: Exec= outside [Desktop Entry]");
            }
            if target.is_some() {
                bail!("unsupported session file: more than one Exec=");
            }
            let rest = rest.trim();
            let (cmd, args) = match rest.split_once(char::is_whitespace) {
                Some((c, a)) => (c, a.trim()),
                None => (rest, ""),
            };
            if cmd.is_empty() || cmd.starts_with('"') || cmd.contains('%') {
                bail!("unsupported Exec= line: {rest}");
            }
            target = Some(resolve(cmd)?);
            out.push_str("Exec=");
            out.push_str(wrapper);
            if !args.is_empty() {
                out.push(' ');
                out.push_str(args);
            }
        } else if line.starts_with("TryExec=") && in_entry {
            out.push_str("TryExec=");
            out.push_str(wrapper);
        } else if in_entry && line.starts_with("Name") && line.split('=').next().is_some_and(|k| k == "Name" || k.starts_with("Name[")) && line.contains('=') {
            out.push_str(line.trim_end());
            out.push_str(" (Forge)");
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !seen_entry {
        bail!("not a session file: no [Desktop Entry] group");
    }
    match target {
        Some(target) => Ok(Rewritten { content: out, target }),
        None => bail!("session file has no Exec= line"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HYPR: &str = "[Desktop Entry]\nName=Hyprland\nName[de]=Hyprland DE\nComment=A compositor\nExec=Hyprland --flag x\nTryExec=Hyprland\nType=Application\nDesktopNames=Hyprland\n";

    #[test]
    fn rewrites_exec_name_and_tryexec() {
        let mut seen = Vec::new();
        let r = rewrite_session(HYPR, "/f/integration/p/h-session", &mut |c| {
            seen.push(c.to_string());
            Ok(format!("/f/bin/{c}"))
        })
        .unwrap();
        assert_eq!(seen, vec!["Hyprland"]);
        assert_eq!(r.target, "/f/bin/Hyprland");
        assert!(r.content.contains("Exec=/f/integration/p/h-session --flag x\n"), "{}", r.content);
        assert!(r.content.contains("TryExec=/f/integration/p/h-session\n"));
        assert!(r.content.contains("Name=Hyprland (Forge)\n"));
        assert!(r.content.contains("Name[de]=Hyprland DE (Forge)\n"));
        assert!(r.content.contains("Comment=A compositor\n"), "other keys untouched");
        assert!(r.content.contains("DesktopNames=Hyprland\n"));
    }

    #[test]
    fn rejects_what_it_cannot_handle_safely() {
        let w = "/w";
        let mut ok = |c: &str| Ok(c.to_string());
        for bad in [
            "[Desktop Entry]\nName=x\n",                                  // no Exec
            "Exec=x\n",                                                   // no group
            "[Desktop Entry]\nExec=a\nExec=b\n",                          // two Exec
            "[Desktop Entry]\nExec=\"a b\" c\n",                          // quoted command
            "[Desktop Entry]\nExec=a %U\nName=x\n[Desktop Action z]\nExec=b\n", // Exec in other group
            "[Desktop Entry]\nExec=a%f\n",                                // field code in command
        ] {
            assert!(rewrite_session(bad, w, &mut ok).is_err(), "{bad:?}");
        }
        assert!(rewrite_session("[Desktop Entry]\nExec=a\n", "/has space/w", &mut ok).is_err());
        // A command the package does not provide is the resolver's call.
        let mut no = |c: &str| -> Result<String> { bail!("no {c}") };
        assert!(rewrite_session("[Desktop Entry]\nExec=a\n", w, &mut no).is_err());
    }

    #[test]
    fn path_chars() {
        assert!(safe_path_chars("/home/u/.local/share/forge/bin/x-1.2+3"));
        for bad in ["", "/a b", "/a\"b", "/a$b", "/a'b", "/a;b", "/a\nb"] {
            assert!(!safe_path_chars(bad), "{bad:?}");
        }
    }
}
