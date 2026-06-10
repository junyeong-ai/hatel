//! `service` — install or remove the receiver as a per-user background service so native OTel is
//! captured gap-free, not only while `serve` runs in a terminal. macOS uses a launchd LaunchAgent,
//! Linux a systemd `--user` unit; the unit runs `serve --all` from this exact binary. Like `init`,
//! the binary owns this OS integration (rather than a copy-pasted plist/unit), so it is consistent,
//! idempotent, and `--print`-able for managed or customized setups. Other platforms are reported
//! honestly as unsupported — run `serve --all` under your own supervisor.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::Write as _;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};

/// launchd label / systemd unit name. Short, matching the command.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const SERVICE_NAME: &str = "hatel";

pub fn run(remove: bool, print: bool) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("service: cannot resolve this binary's path: {e}");
            return 1;
        }
    };

    #[cfg(target_os = "macos")]
    return macos(&exe, remove, print);
    #[cfg(target_os = "linux")]
    return linux(&exe, remove, print);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (&exe, remove, print);
        eprintln!(
            "service: automated install is supported on macOS (launchd) and Linux (systemd --user) \
             only; run `{} serve --all` under your platform's service manager",
            exe.display()
        );
        1
    }
}

#[cfg(target_os = "macos")]
fn macos(exe: &Path, remove: bool, print: bool) -> i32 {
    let label = format!("dev.{SERVICE_NAME}");
    let exe_xml = xml_escape(&exe.display().to_string());
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe_xml}</string><string>serve</string><string>--all</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
"#
    );
    if print {
        print!("{plist}");
        return 0;
    }
    let Some(home) = crate::claude_settings::home_dir() else {
        eprintln!("service: no home directory");
        return 1;
    };
    let path = home.join(format!("Library/LaunchAgents/{label}.plist"));

    if remove {
        quiet(Command::new("launchctl").arg("unload").arg(&path));
        return remove_unit(&path);
    }

    if let Err(e) = write_unit(&path, plist.as_bytes()) {
        eprintln!("service: writing {}: {e}", path.display());
        return 1;
    }
    // Reload so a re-run picks up a new path: unload first (ignore "not loaded"), then load.
    quiet(Command::new("launchctl").arg("unload").arg(&path));
    match Command::new("launchctl").arg("load").arg("-w").arg(&path).status() {
        Ok(s) if s.success() => {
            println!("installed and loaded the launchd agent: {}", path.display());
            println!("the receiver now starts at login and is kept alive");
            0
        }
        Ok(s) => {
            eprintln!("service: `launchctl load` exited {s}; the plist is at {}", path.display());
            1
        }
        Err(e) => {
            eprintln!("service: launchctl not found ({e}); the plist is at {}", path.display());
            1
        }
    }
}

#[cfg(target_os = "linux")]
fn linux(exe: &Path, remove: bool, print: bool) -> i32 {
    let exec = systemd_exec_arg(&exe.display().to_string());
    let unit = format!(
        "[Unit]\nDescription={SERVICE_NAME} receiver\n\n[Service]\nExecStart={exec} serve --all\nRestart=always\n\n[Install]\nWantedBy=default.target\n"
    );
    if print {
        print!("{unit}");
        return 0;
    }
    let Some(home) = crate::claude_settings::home_dir() else {
        eprintln!("service: no home directory");
        return 1;
    };
    let svc = format!("{SERVICE_NAME}.service");
    let path = home.join(format!(".config/systemd/user/{svc}"));

    if remove {
        quiet(Command::new("systemctl").args(["--user", "disable", "--now", &svc]));
        let r = remove_unit(&path);
        quiet(Command::new("systemctl").args(["--user", "daemon-reload"]));
        return r;
    }

    if let Err(e) = write_unit(&path, unit.as_bytes()) {
        eprintln!("service: writing {}: {e}", path.display());
        return 1;
    }
    quiet(Command::new("systemctl").args(["--user", "daemon-reload"]));
    // `enable` sets login autostart; `restart` is the operative start that also repoints a unit
    // already running to the freshly-written ExecStart. Both must succeed for genuinely gap-free
    // collection (survives reboot AND runs now), so neither failure is swallowed.
    for action in ["enable", "restart"] {
        match Command::new("systemctl").args(["--user", action, svc.as_str()]).status() {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("service: `systemctl --user {action}` exited {s}; the unit is at {}", path.display());
                return 1;
            }
            Err(e) => {
                eprintln!("service: systemctl not found ({e}); the unit is at {}", path.display());
                return 1;
            }
        }
    }
    println!("installed and started the systemd user unit: {}", path.display());
    println!("the receiver now starts on login and is restarted on failure");
    0
}

/// Escape a string for XML element text — the binary path goes inside `<string>…</string>`, so a
/// path containing `&`, `<`, or `>` must be escaped to stay valid (and uninjectable).
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Render a path as a single systemd `ExecStart` token: quote it (systemd splits on whitespace)
/// and double any `%` (a systemd specifier), escaping embedded `\` and `"` per its quoting rules —
/// so a path with spaces or `%` runs correctly.
#[cfg(target_os = "linux")]
fn systemd_exec_arg(path: &str) -> String {
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
    format!("\"{escaped}\"")
}

/// Run a best-effort command (unload/disable/reload) discarding its output — these legitimately
/// fail when nothing is installed yet, and the loader's stderr would just be confusing noise.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn quiet(cmd: &mut Command) {
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_unit(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path)?.write_all(bytes)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn remove_unit(path: &Path) -> i32 {
    match std::fs::remove_file(path) {
        Ok(()) => {
            println!("removed {}", path.display());
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("service not installed (nothing at {})", path.display());
            0
        }
        Err(e) => {
            eprintln!("service: removing {}: {e}", path.display());
            1
        }
    }
}
