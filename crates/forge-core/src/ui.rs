//! Terminal output: colour, symbols, a progress spinner and small formatters.
//!
//! Everything degrades to plain text when stdout isn't a terminal (scripts,
//! logs, CI), when `NO_COLOR` is set or `TERM=dumb`. `FORGE_COLOR=always|never`
//! forces colour, and `FORGE_ASCII=1` forces ASCII symbols.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Style {
    color: bool,
    unicode: bool,
    /// Animate in place (a real terminal that understands cursor control).
    tty: bool,
}

fn style() -> &'static Style {
    static S: OnceLock<Style> = OnceLock::new();
    S.get_or_init(|| {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let dumb = env("TERM").as_deref() == Some("dumb");
        let tty = std::io::stdout().is_terminal() && !dumb;
        let color = match env("FORGE_COLOR").as_deref() {
            Some("always") => true,
            Some("never") => false,
            _ => tty && env("NO_COLOR").is_none(),
        };
        let utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|k| env(k))
            .is_some_and(|v| {
                let v = v.to_lowercase();
                v.contains("utf-8") || v.contains("utf8")
            });
        Style { color, unicode: utf8 && env("FORGE_ASCII").is_none(), tty }
    })
}

pub fn paint(code: &str, s: &str) -> String {
    if style().color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}

fn pick(unicode: &'static str, ascii: &'static str) -> &'static str {
    if style().unicode { unicode } else { ascii }
}
pub fn check() -> String {
    green(pick("✓", "ok"))
}
pub fn cross() -> String {
    red(pick("✗", "FAILED"))
}
pub fn bullet() -> String {
    cyan(pick("◆", "*"))
}
/// Marks something the user should look at but that isn't an error.
pub fn warn() -> String {
    yellow("!")
}
pub fn arrow() -> &'static str {
    pick("→", "->")
}
pub fn dot() -> &'static str {
    pick("·", ",")
}

/// `1.3s`, `1m05s`
pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 { format!("{s:.1}s") } else { format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60) }
}

/// `32.1 MB`, `512 B`
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

/// Pad (or leave) a plain string to `width` characters. Pass text before styling it.
pub fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(n)))
}

/// Replace the home directory prefix with `~`.
pub fn tilde(path: &Path) -> String {
    let p = path.to_string_lossy();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && (p == h || p.starts_with(&format!("{h}/"))) => format!("~{}", &p[h.len()..]),
        _ => p.into_owned(),
    }
}

/// `https://github.com/a/b.git` -> `github.com/a/b`, for display only.
pub fn short_url(url: &str) -> String {
    let u = url.trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    for scheme in ["https://", "http://", "git://", "ssh://"] {
        if let Some(rest) = u.strip_prefix(scheme) {
            return rest.strip_prefix("git@").unwrap_or(rest).to_string();
        }
    }
    u.strip_prefix("git@").map(|r| r.replacen(':', "/", 1)).unwrap_or_else(|| u.to_string())
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    format!("{}…", s.chars().take(keep).collect::<String>())
}

fn terminal_width() -> usize {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ fills a plain struct.
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 20 { ws.ws_col as usize } else { 80 }
}

/// Column at which step timings line up (labels are at most about this wide).
const STEP_COLUMN: usize = 44;

/// One finished step: `  ✓ compile                     12.2s`
pub fn step_line(ok: bool, label: &str, note: &str, elapsed: Duration) -> String {
    let mark = if ok { check() } else { cross() };
    let pad = " ".repeat(STEP_COLUMN.saturating_sub(label.chars().count()).max(2));
    let time = dim(&fmt_dur(elapsed));
    if note.is_empty() {
        format!("  {mark} {label}{pad}{time}")
    } else {
        format!("  {mark} {label}{pad}{time}  {}", dim(note))
    }
}

/// A step in progress. On a terminal it animates and can show a detail
/// (the latest line of build output); otherwise it prints only the result.
pub struct Spinner {
    label: String,
    start: Instant,
    detail: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    /// `animate = false` for steps whose own output is on screen (`--verbose`).
    pub fn start(label: &str, animate: bool) -> Spinner {
        let detail = Arc::new(Mutex::new(String::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = (animate && style().tty).then(|| {
            let (label, detail, stop) = (label.to_string(), detail.clone(), stop.clone());
            std::thread::spawn(move || {
                let frames: &[&str] =
                    if style().unicode { &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] } else { &["|", "/", "-", "\\"] };
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    let d = detail.lock().map(|d| d.clone()).unwrap_or_default();
                    let width = terminal_width();
                    let head_len = 4 + label.chars().count();
                    let mut line = format!("  {} {}", cyan(frames[i % frames.len()]), label);
                    if !d.is_empty() && width > head_len + 12 {
                        line.push_str(&format!("  {}", dim(&truncate(&d, width - head_len - 4))));
                    }
                    print!("\r\x1b[K{line}");
                    let _ = std::io::stdout().flush();
                    i += 1;
                    std::thread::sleep(Duration::from_millis(80));
                }
            })
        });
        Spinner { label: label.to_string(), start: Instant::now(), detail, stop, handle }
    }

    pub fn set_detail(&self, s: &str) {
        if let Ok(mut d) = self.detail.lock() {
            *d = s.to_string();
        }
    }

    fn halt(&mut self) {
        if let Some(h) = self.handle.take() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = h.join();
            print!("\r\x1b[K");
            let _ = std::io::stdout().flush();
        }
    }

    pub fn finish(mut self, ok: bool, note: &str) {
        self.halt();
        println!("{}", step_line(ok, &self.label, note, self.start.elapsed()));
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.halt(); // an early return must not leave the animation running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_dur(Duration::from_millis(3340)), "3.3s");
        assert_eq!(fmt_dur(Duration::from_secs(65)), "1m05s");
        assert_eq!(fmt_dur(Duration::from_secs(600)), "10m00s");
    }

    #[test]
    fn sizes_and_padding() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(33_554_432), "32.0 MB");
        assert_eq!(pad("ab", 5), "ab   ");
        assert_eq!(pad("abcdef", 3), "abcdef");
    }

    #[test]
    fn urls() {
        assert_eq!(short_url("https://github.com/fastfetch-cli/fastfetch"), "github.com/fastfetch-cli/fastfetch");
        assert_eq!(short_url("https://github.com/a/b.git/"), "github.com/a/b");
        assert_eq!(short_url("git@github.com:a/b.git"), "github.com/a/b");
        assert_eq!(short_url("/home/u/repo"), "/home/u/repo");
    }

    #[test]
    fn truncation_and_alignment() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        // Plain output when not on a terminal: label, padding, time.
        let l = step_line(true, "compile", "", Duration::from_millis(12200));
        assert!(l.contains("compile") && l.ends_with("12.2s"), "{l}");
        let long = step_line(true, &"x".repeat(40), "", Duration::from_secs(1));
        assert!(long.contains(&format!("{}  ", "x".repeat(40))), "{long}");
        assert!(step_line(false, "x", "boom", Duration::from_secs(1)).contains("boom"));
    }

    #[test]
    fn tilde_replaces_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(tilde(Path::new(&format!("{home}/a/b"))), "~/a/b");
        assert_eq!(tilde(Path::new(&home)), "~");
        assert_eq!(tilde(Path::new("/usr/bin")), "/usr/bin");
        assert_eq!(tilde(Path::new(&format!("{home}x/a"))), format!("{home}x/a"));
    }
}
