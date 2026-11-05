//! Running build commands under Bubblewrap.
//!
//! The sandbox sees: a read-only `/usr` and `/etc`, a private `/tmp`, `/proc`
//! and `/dev`, and read-write only the job's own directories. No `$HOME`, no
//! other Forge packages, no network unless the phase asks for it, and the
//! process dies with its parent. Resource limits: wall-clock timeout, no core
//! dumps, capped file size.

use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SANDBOX_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const MAX_FILE_SIZE: u64 = 16 << 30;

/// Can we create the sandbox on this machine?
pub fn check() -> Result<(), String> {
    let out = Command::new("bwrap")
        .args(["--unshare-all", "--ro-bind", "/", "/", "--dev", "/dev", "true"])
        .output()
        .map_err(|e| format!("cannot run bwrap (install bubblewrap): {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "bwrap cannot create a sandbox: {}\n\
             (unprivileged user namespaces may be disabled, e.g. by AppArmor on Ubuntu 24.04+)",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub struct Run<'a> {
    pub script: &'a str,
    /// Working directory *inside* the sandbox.
    pub cwd: &'a str,
    pub env: &'a [(String, String)],
    /// Read-write mounts `(host path, path inside)`.
    pub binds: &'a [(PathBuf, String)],
    /// Read-only mounts `(host path, path inside)`, e.g. installed dependencies.
    pub ro_binds: &'a [(PathBuf, String)],
    pub network: bool,
    pub log: &'a Path,
    pub verbose: bool,
    /// Shown while the command runs (receives the latest output line).
    pub spinner: Option<&'a forge_core::ui::Spinner>,
    pub timeout: Duration,
    /// Run without bwrap (explicit opt-out). `binds` are ignored and `cwd` is a host path.
    pub insecure: bool,
}

fn bwrap_args(r: &Run) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |s: &str| a.push(s.to_string());
    push("--unshare-all");
    if r.network {
        push("--share-net");
    }
    for s in ["--die-with-parent", "--new-session", "--clearenv", "--ro-bind", "/usr", "/usr"] {
        push(s);
    }
    for d in ["/bin", "/sbin", "/lib", "/lib32", "/lib64"] {
        match fs::read_link(d) {
            Ok(t) => {
                push("--symlink");
                push(&t.to_string_lossy());
                push(d);
            }
            Err(_) if Path::new(d).exists() => {
                push("--ro-bind");
                push(d);
                push(d);
            }
            Err(_) => {}
        }
    }
    for s in ["--ro-bind", "/etc", "/etc"] {
        push(s);
    }
    if r.network {
        for s in ["--ro-bind-try", "/run/systemd/resolve", "/run/systemd/resolve"] {
            push(s);
        }
    }
    for s in ["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"] {
        push(s);
    }
    for (host, inside) in r.binds {
        push("--bind");
        push(&host.to_string_lossy());
        push(inside);
    }
    for (host, inside) in r.ro_binds {
        push("--ro-bind");
        push(&host.to_string_lossy());
        push(inside);
    }
    push("--chdir");
    push(r.cwd);
    for (k, v) in r.env {
        push("--setenv");
        push(k);
        push(v);
    }
    for s in ["--", "sh", "-ec", r.script] {
        push(s);
    }
    a
}

/// Run one shell script; output goes to the log (or the terminal if verbose).
pub fn run(r: &Run) -> Result<()> {
    let mut log = OpenOptions::new().create(true).append(true).open(r.log)?;
    writeln!(log, "\n$ {}", r.script)?;

    let mut cmd = if r.insecure {
        let mut c = Command::new("sh");
        c.args(["-ec", r.script]).current_dir(r.cwd).env_clear();
        for (k, v) in r.env {
            c.env(k, v);
        }
        c
    } else {
        let mut c = Command::new("bwrap");
        c.args(bwrap_args(r));
        c
    };
    if r.verbose {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        cmd.stdout(log.try_clone()?).stderr(log.try_clone()?);
    }
    cmd.stdin(Stdio::null());
    // SAFETY: only async-signal-safe calls (setrlimit, setsid-equivalent) between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            libc::setrlimit(libc::RLIMIT_CORE, &zero);
            let fsize = libc::rlimit { rlim_cur: MAX_FILE_SIZE, rlim_max: MAX_FILE_SIZE };
            libc::setrlimit(libc::RLIMIT_FSIZE, &fsize);
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting build command")?;
    let deadline = Instant::now() + r.timeout;
    let mut last_tick = Instant::now() - Duration::from_secs(1);
    let mut last_detail = String::new();
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("command failed ({status})");
        }
        // Feed the spinner the build's latest output line ("[37/142] ...").
        if let Some(sp) = r.spinner {
            if last_tick.elapsed() >= Duration::from_millis(250) {
                last_tick = Instant::now();
                let line = tail_line(r.log);
                if line != last_detail {
                    sp.set_detail(&line);
                    last_detail = line;
                }
            }
        }
        if Instant::now() >= deadline {
            // Kill the whole process group; bwrap's --die-with-parent covers the rest.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            bail!("build step exceeded the {}-minute time limit", r.timeout.as_secs() / 60);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The most recent non-empty line of the build log (lines end in \n or \r).
fn tail_line(log: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = fs::File::open(log) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(2048);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf)
        .split(['\n', '\r'])
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("$ "))
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_line_picks_the_latest_progress_line() {
        let p = std::env::temp_dir().join(format!("forge-tail-{}", std::process::id()));
        fs::write(&p, "$ cmake --build\n[1/9] cc a.o\r[2/9] cc b.o\r\n\n").unwrap();
        assert_eq!(tail_line(&p), "[2/9] cc b.o");
        // Our own "$ command" header lines are not build output.
        fs::write(&p, "$ cmake --build x\n").unwrap();
        assert_eq!(tail_line(&p), "");
        fs::write(&p, "").unwrap();
        assert_eq!(tail_line(&p), "");
        fs::remove_file(&p).unwrap();
    }

    #[test]
    fn args_shape() {
        let run = Run {
            script: "echo hi",
            cwd: "/build/src",
            env: &[("A".into(), "b".into())],
            binds: &[(PathBuf::from("/tmp/x"), "/build/src".into())],
            ro_binds: &[(PathBuf::from("/tmp/dep"), "/opt/dep".into())],
            network: false,
            log: Path::new("/dev/null"),
            verbose: false,
            spinner: None,
            timeout: Duration::from_secs(1),
            insecure: false,
        };
        let a = bwrap_args(&run);
        assert!(a.contains(&"--unshare-all".to_string()));
        let i = a.iter().position(|x| x == "/opt/dep").unwrap();
        assert_eq!(a[i - 2], "--ro-bind");
        assert!(!a.contains(&"--share-net".to_string()));
        assert_eq!(a[a.len() - 4..], ["--", "sh", "-ec", "echo hi"].map(String::from));
        let net = Run { network: true, ..run };
        assert!(bwrap_args(&net).contains(&"--share-net".to_string()));
    }
}
