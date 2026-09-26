//! `service` — install or remove the receiver as a per-user background service so native OTel is
//! captured gap-free, not only while `serve` runs in a terminal. macOS uses a launchd LaunchAgent,
//! Linux a systemd `--user` unit; the unit runs `serve --all` from this exact binary. Like `init`,
//! the binary owns this OS integration (rather than a copy-pasted plist/unit), so it is consistent,
//! idempotent, and `--print`-able for managed or customized setups. `--restart` is what an
//! upgrade needs: a running receiver keeps the binary it started from, so the installer restarts
//! the service — and only if one is installed and running, never installing one as a side effect.
//! That restart also brings a unit an earlier release wrote for this binary up to this build's, so
//! a change to the unit reaches an upgraded install; a unit edited by hand or written for another
//! binary is kept. The unit carries nothing to configure: the receiver reads its settings from
//! `config.toml`, as every other process does. Other platforms are reported honestly as
//! unsupported — run `serve --all` under your own supervisor.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::Write as _;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};

/// launchd label / systemd unit name. Short, matching the command.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const SERVICE_NAME: &str = "hatel";

/// How long the service manager waits after SIGTERM before killing the receiver: the export drain's
/// bound plus room for the requests in flight and the final flush ahead of it. The units declare
/// it because a manager's own default can be shorter than the drain (launchd gives a user agent
/// 5 s), and a stop killed while draining loses the batches it was about to deliver.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const STOP_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(crate::serve::EXPORT_DRAIN_TIMEOUT.as_secs() + 10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Remove,
    Print,
    Restart,
}

pub fn run(action: Action) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("service: cannot resolve this binary's path: {e}");
            return 1;
        }
    };

    #[cfg(target_os = "macos")]
    return macos(&exe, action);
    #[cfg(target_os = "linux")]
    return linux(&exe, action);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if action == Action::Restart {
            println!("no receiver service to restart on this platform");
            return 0;
        }
        eprintln!(
            "service: automated install is supported on macOS (launchd) and Linux (systemd --user) \
             only; run `{} serve --all` under your platform's service manager",
            exe.display()
        );
        1
    }
}

#[cfg(target_os = "macos")]
fn macos(exe: &Path, action: Action) -> i32 {
    let label = format!("dev.{SERVICE_NAME}");
    let Some(home) = crate::claude_settings::home_dir() else {
        eprintln!("service: no home directory");
        return 1;
    };
    let log = home.join(format!("Library/Logs/{SERVICE_NAME}/serve.log"));
    let plist = launchd_plist(&label, exe, &log);
    if action == Action::Print {
        print!("{plist}");
        return 0;
    }
    let path = home.join(format!("Library/LaunchAgents/{label}.plist"));

    if action == Action::Remove {
        quiet(Command::new("launchctl").arg("unload").arg(&path));
        return remove_unit(&path);
    }
    if action == Action::Restart {
        let installed = match installed_unit(&path, &plist, &earlier_launchd_plists(&label, exe)) {
            Ok(Some(installed)) => installed,
            Ok(None) => {
                println!("no receiver service installed");
                return 0;
            }
            Err(e) => {
                eprintln!("service: {e}");
                return 1;
            }
        };
        if !receiver_would_start() {
            return 1;
        }
        if installed == Installed::Other {
            note_kept_unit(&path);
        }
        if installed == Installed::Earlier
            && let Err(e) = write_launchd_unit(&path, &plist, &log)
        {
            eprintln!("service: {e}");
            return 1;
        }
        // SAFETY: `getuid` has no preconditions and cannot fail.
        let target = format!("gui/{}/{label}", unsafe { libc::getuid() });
        // `print` answers only for a loaded service; an installed-but-unloaded one was stopped
        // on purpose, and a restart is not the place to undo that. launchd reads the plist when
        // it next loads the job.
        if !succeeds(Command::new("launchctl").args(["print", &target])) {
            if installed == Installed::Earlier {
                println!("updated the service unit to this build's");
            }
            if installed == Installed::Other {
                println!(
                    "the receiver service is installed but not loaded; `launchctl load -w {}` loads \
                     it as it is",
                    path.display()
                );
            } else {
                println!(
                    "the receiver service is installed but not loaded; `hatel service` loads it"
                );
            }
            return 0;
        }
        // launchd reads a plist only when it loads the job, so a replaced one takes effect
        // through a reload, which also restarts the receiver.
        if installed == Installed::Earlier {
            return match reload(&path) {
                Ok(()) => {
                    println!(
                        "updated the service unit to this build's and restarted the receiver; \
                         `hatel doctor` says which build answers"
                    );
                    0
                }
                Err(e) => {
                    eprintln!("service: {e}");
                    1
                }
            };
        }
        return match Command::new("launchctl")
            .args(["kickstart", "-k", &target])
            .status()
        {
            Ok(s) if s.success() => {
                println!("restarted the receiver service; `hatel doctor` says which build answers");
                0
            }
            Ok(s) => {
                eprintln!("service: `launchctl kickstart -k {target}` exited {s}");
                1
            }
            Err(e) => {
                eprintln!("service: launchctl not found ({e})");
                1
            }
        };
    }

    if !receiver_would_start() {
        return 1;
    }
    match write_launchd_unit(&path, &plist, &log).and_then(|()| reload(&path)) {
        Ok(()) => {
            println!("installed and loaded the launchd agent: {}", path.display());
            println!(
                "the receiver now starts at login and is kept alive; it logs to {}",
                log.display()
            );
            0
        }
        Err(e) => {
            eprintln!("service: {e}");
            1
        }
    }
}

/// The LaunchAgent that runs `serve --all` from `exe`. launchd discards a job's output unless the
/// plist names a file, so the receiver's stdout and stderr go to `log`; and it waits
/// [`STOP_TIMEOUT`] after SIGTERM, not its own shorter default, so a
/// stop can finish draining egress.
#[cfg(target_os = "macos")]
fn launchd_plist(label: &str, exe: &Path, log: &Path) -> String {
    let exe_xml = xml_escape(&exe.display().to_string());
    let log_xml = xml_escape(&log.display().to_string());
    let stop = STOP_TIMEOUT.as_secs();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe_xml}</string><string>serve</string><string>--all</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ExitTimeOut</key><integer>{stop}</integer>
  <key>StandardOutPath</key><string>{log_xml}</string>
  <key>StandardErrorPath</key><string>{log_xml}</string>
</dict></plist>
"#
    )
}

/// The plists earlier releases wrote for `exe` (v0.4.0 to v0.18.1): one still byte-equal to them is
/// hatel's own and unedited. Changing [`launchd_plist`] adds the rendering it replaces here, which
/// `the_plist_this_build_writes_is_pinned` enforces.
#[cfg(target_os = "macos")]
fn earlier_launchd_plists(label: &str, exe: &Path) -> Vec<String> {
    let exe_xml = xml_escape(&exe.display().to_string());
    vec![format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe_xml}</string><string>serve</string><string>--all</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
</dict></plist>
"#
    )]
}

/// Write the plist, and create the directory its log goes to, so the log does not depend on
/// launchd creating a missing directory.
#[cfg(target_os = "macos")]
fn write_launchd_unit(path: &Path, plist: &str, log: &Path) -> Result<(), String> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    write_unit(path, plist.as_bytes()).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Unload the agent if loaded (a stop, on SIGTERM) and load it again, which starts the receiver
/// from the plist on disk.
#[cfg(target_os = "macos")]
fn reload(path: &Path) -> Result<(), String> {
    quiet(Command::new("launchctl").arg("unload").arg(path));
    match Command::new("launchctl")
        .arg("load")
        .arg("-w")
        .arg(path)
        .status()
    {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "`launchctl load` exited {s}; the plist is at {}",
            path.display()
        )),
        Err(e) => Err(format!(
            "launchctl not found ({e}); the plist is at {}",
            path.display()
        )),
    }
}

#[cfg(target_os = "linux")]
fn linux(exe: &Path, action: Action) -> i32 {
    let unit = systemd_unit(exe);
    if action == Action::Print {
        print!("{unit}");
        return 0;
    }
    let Some(home) = crate::claude_settings::home_dir() else {
        eprintln!("service: no home directory");
        return 1;
    };
    let svc = format!("{SERVICE_NAME}.service");
    let path = home.join(format!(".config/systemd/user/{svc}"));

    if action == Action::Remove {
        quiet(Command::new("systemctl").args(["--user", "disable", "--now", &svc]));
        let r = remove_unit(&path);
        quiet(Command::new("systemctl").args(["--user", "daemon-reload"]));
        return r;
    }
    if action == Action::Restart {
        let installed = match installed_unit(&path, &unit, &earlier_systemd_units(exe)) {
            Ok(Some(installed)) => installed,
            Ok(None) => {
                println!("no receiver service installed");
                return 0;
            }
            Err(e) => {
                eprintln!("service: {e}");
                return 1;
            }
        };
        if !receiver_would_start() {
            return 1;
        }
        if installed == Installed::Other {
            note_kept_unit(&path);
        }
        // systemd reads a unit file on daemon-reload, so a replaced one is reloaded before the
        // restart that applies it. A manager out of reach from here reads the file when it starts.
        let reloaded = if installed == Installed::Earlier {
            if let Err(e) = write_unit(&path, unit.as_bytes()) {
                eprintln!("service: writing {}: {e}", path.display());
                return 1;
            }
            println!("updated the service unit to this build's");
            daemon_reload()
        } else {
            Ok(())
        };
        // A unit that is not active was stopped on purpose, or its manager is out of reach from
        // here (no session bus); a restart is not the place to start one or to fail an install.
        if !succeeds(Command::new("systemctl").args(["--user", "is-active", "--quiet", &svc])) {
            let start = if installed == Installed::Other {
                format!("`systemctl --user start {svc}` starts it as it is")
            } else {
                "`hatel service` starts it".to_string()
            };
            println!(
                "the receiver service is installed but not running (or its manager is not \
                 reachable from here); {start}"
            );
            if let Err(e) = reloaded {
                println!(
                    "systemd has not read the updated unit yet ({e}); it reads it at the next \
                     `systemctl --user daemon-reload`, or when the user manager next starts"
                );
            }
            return 0;
        }
        // A restart after a failed reload would run the definition systemd loaded before.
        if let Err(e) = reloaded {
            eprintln!("service: {e}; the receiver keeps the unit systemd loaded before");
            return 1;
        }
        return match Command::new("systemctl")
            .args(["--user", "try-restart", &svc])
            .status()
        {
            Ok(s) if s.success() => {
                println!("restarted the receiver service; `hatel doctor` says which build answers");
                0
            }
            Ok(s) => {
                eprintln!("service: `systemctl --user try-restart {svc}` exited {s}");
                1
            }
            Err(e) => {
                eprintln!("service: systemctl not found ({e})");
                1
            }
        };
    }

    if !receiver_would_start() {
        return 1;
    }
    if let Err(e) = write_unit(&path, unit.as_bytes()) {
        eprintln!("service: writing {}: {e}", path.display());
        return 1;
    }
    if let Err(e) = daemon_reload() {
        eprintln!("service: {e}; the unit is at {}", path.display());
        return 1;
    }
    // `enable` sets login autostart; `restart` is the operative start that also repoints a unit
    // already running to the freshly-written ExecStart. Both must succeed for genuinely gap-free
    // collection (survives reboot AND runs now), so neither failure is swallowed.
    for action in ["enable", "restart"] {
        match Command::new("systemctl")
            .args(["--user", action, svc.as_str()])
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!(
                    "service: `systemctl --user {action}` exited {s}; the unit is at {}",
                    path.display()
                );
                return 1;
            }
            Err(e) => {
                eprintln!(
                    "service: systemctl not found ({e}); the unit is at {}",
                    path.display()
                );
                return 1;
            }
        }
    }
    println!(
        "installed and started the systemd user unit: {}",
        path.display()
    );
    println!("the receiver now starts on login and is restarted on failure");
    0
}

/// The systemd user unit that runs `serve --all` from `exe`, waiting
/// [`STOP_TIMEOUT`] after SIGTERM so a stop can finish draining egress.
#[cfg(target_os = "linux")]
fn systemd_unit(exe: &Path) -> String {
    let exec = systemd_exec_arg(&exe.display().to_string());
    let stop = STOP_TIMEOUT.as_secs();
    format!(
        "[Unit]\nDescription={SERVICE_NAME} receiver\n\n[Service]\nExecStart={exec} serve --all\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec={stop}\n\n[Install]\nWantedBy=default.target\n"
    )
}

/// The units earlier releases wrote for `exe` (v0.4.0 to v0.18.1): one still byte-equal to them is
/// hatel's own and unedited. Changing [`systemd_unit`] adds the rendering it replaces here, which
/// `the_systemd_unit_this_build_writes_is_pinned` enforces.
#[cfg(target_os = "linux")]
fn earlier_systemd_units(exe: &Path) -> Vec<String> {
    let exec = systemd_exec_arg(&exe.display().to_string());
    vec![format!(
        "[Unit]\nDescription={SERVICE_NAME} receiver\n\n[Service]\nExecStart={exec} serve --all\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n"
    )]
}

#[cfg(target_os = "linux")]
fn daemon_reload() -> Result<(), String> {
    match Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
    {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("`systemctl --user daemon-reload` exited {s}")),
        Err(e) => Err(format!("systemctl not found ({e})")),
    }
}

/// How an installed unit relates to the one this build writes.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Installed {
    /// Byte-equal to this build's.
    Current,
    /// Byte-equal to what an earlier release wrote for this binary: hatel's own and unedited, so
    /// replacing it loses nothing.
    Earlier,
    /// Anything else — edited by hand, or written for another binary. Replacing it would drop the
    /// edit or repoint the service, so it is left as it is.
    Other,
}

/// Classify the unit at `path` against `current` and the `earlier` renderings for this binary;
/// `None` when no unit is installed.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn installed_unit(
    path: &Path,
    current: &str,
    earlier: &[String],
) -> Result<Option<Installed>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    Ok(Some(if bytes == current.as_bytes() {
        Installed::Current
    } else if earlier.iter().any(|e| bytes == e.as_bytes()) {
        Installed::Earlier
    } else {
        Installed::Other
    }))
}

/// Whether a receiver started now would serve: the checks `serve` makes first, as this process
/// resolves the configuration — the file a service manager's receiver reads too, unless
/// `HATEL_CONFIG` or `XDG_CONFIG_HOME` differs between them. Asked before anything changes, since
/// stopping a running receiver for one that exits is an outage.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn receiver_would_start() -> bool {
    match crate::serve::startup() {
        Ok(_) => true,
        Err(e) => {
            eprintln!("service: a receiver started now would exit ({e}); nothing was changed");
            false
        }
    }
}

/// Say that a restart kept a unit differing from this build's, and how to replace it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn note_kept_unit(path: &Path) {
    println!(
        "kept the installed unit {}, which differs from the one this build writes. `hatel service` \
         replaces it with this build's, which sets no environment, so first move what it sets into \
         the default config.toml: HATEL_PLUGINS as `plugins`, storage variables under [storage], \
         and a HATEL_CONFIG file's contents into that file (`hatel service --print` shows this \
         build's unit)",
        path.display()
    );
}

/// Escape a string for XML element text — the binary path goes inside `<string>…</string>`, so a
/// path containing `&`, `<`, or `>` must be escaped to stay valid (and uninjectable).
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a path as a single systemd `ExecStart` token: quote it (systemd splits on whitespace)
/// and double any `%` (a systemd specifier), escaping embedded `\` and `"` per its quoting rules —
/// so a path with spaces or `%` runs correctly.
#[cfg(target_os = "linux")]
fn systemd_exec_arg(path: &str) -> String {
    let escaped = path
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// Run a best-effort command (unload/disable/reload) discarding its output — these legitimately
/// fail when nothing is installed yet, and the loader's stderr would just be confusing noise.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn quiet(cmd: &mut Command) {
    let _ = succeeds(cmd);
}

/// Whether a command exits zero, its output discarded — for a question asked of the service
/// manager, where the exit code is the answer.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn succeeds(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn the_launchd_agent_waits_out_a_draining_stop_and_keeps_its_output() {
        let log = Path::new("/Users/u/Library/Logs/hatel/serve.log");
        let plist = launchd_plist("dev.hatel", Path::new("/opt/a&b/hatel"), log);
        let stop = STOP_TIMEOUT.as_secs();
        assert!(plist.contains(&format!("<key>ExitTimeOut</key><integer>{stop}</integer>")));
        for key in ["StandardOutPath", "StandardErrorPath"] {
            assert!(plist.contains(&format!(
                "<key>{key}</key><string>{}</string>",
                log.display()
            )));
        }
        assert!(plist.contains("<string>/opt/a&amp;b/hatel</string>"));
    }

    /// What v0.18.1 wrote as `/Users/u/.local/bin/hatel`'s agent: an installed plist, copied.
    #[cfg(target_os = "macos")]
    const PLIST_V0_18_1: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.hatel</string>
  <key>ProgramArguments</key>
  <array><string>/Users/u/.local/bin/hatel</string><string>serve</string><string>--all</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
</dict></plist>
"#;

    #[cfg(target_os = "macos")]
    #[test]
    fn a_restart_replaces_only_a_plist_an_earlier_release_wrote_for_this_binary() {
        let exe = Path::new("/Users/u/.local/bin/hatel");
        let log = Path::new("/Users/u/Library/Logs/hatel/serve.log");
        let current = launchd_plist("dev.hatel", exe, log);
        let earlier = earlier_launchd_plists("dev.hatel", exe);
        let dir = std::env::temp_dir().join(format!("ht-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dev.hatel.plist");
        let classify = |text: &str| {
            std::fs::write(&path, text).unwrap();
            installed_unit(&path, &current, &earlier).unwrap()
        };
        assert_eq!(classify(&current), Some(Installed::Current));
        assert_eq!(classify(PLIST_V0_18_1), Some(Installed::Earlier));
        let edited = PLIST_V0_18_1.replace(
            "  <key>RunAtLoad</key>",
            "  <key>EnvironmentVariables</key><dict><key>HATEL_RETENTION_DAYS</key>\
             <string>365</string></dict>\n  <key>RunAtLoad</key>",
        );
        assert_eq!(
            classify(&edited),
            Some(Installed::Other),
            "a hand edit is kept"
        );
        let checkout =
            PLIST_V0_18_1.replace("/Users/u/.local/bin/hatel", "/src/target/debug/hatel");
        assert_eq!(
            classify(&checkout),
            Some(Installed::Other),
            "a unit for another binary keeps pointing where it did"
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(installed_unit(&path, &current, &earlier).unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_plist_this_build_writes_is_pinned() {
        // A restart replaces only a unit byte-equal to an earlier rendering, so a change to this
        // one also moves it into `earlier_launchd_plists`; otherwise an upgrade keeps every
        // installed unit as it is.
        let plist = launchd_plist(
            "dev.hatel",
            Path::new("/Users/u/.local/bin/hatel"),
            Path::new("/Users/u/Library/Logs/hatel/serve.log"),
        );
        assert_eq!(
            plist,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.hatel</string>
  <key>ProgramArguments</key>
  <array><string>/Users/u/.local/bin/hatel</string><string>serve</string><string>--all</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ExitTimeOut</key><integer>15</integer>
  <key>StandardOutPath</key><string>/Users/u/Library/Logs/hatel/serve.log</string>
  <key>StandardErrorPath</key><string>/Users/u/Library/Logs/hatel/serve.log</string>
</dict></plist>
"#
        );
    }

    /// What v0.18.1 wrote as `/home/u/.local/bin/hatel`'s unit.
    #[cfg(target_os = "linux")]
    const UNIT_V0_18_1: &str = "[Unit]\nDescription=hatel receiver\n\n[Service]\nExecStart=\"/home/u/.local/bin/hatel\" serve --all\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n";

    #[cfg(target_os = "linux")]
    #[test]
    fn a_restart_replaces_only_a_unit_an_earlier_release_wrote_for_this_binary() {
        let exe = Path::new("/home/u/.local/bin/hatel");
        let current = systemd_unit(exe);
        let earlier = earlier_systemd_units(exe);
        let dir = std::env::temp_dir().join(format!("ht-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hatel.service");
        let classify = |text: &str| {
            std::fs::write(&path, text).unwrap();
            installed_unit(&path, &current, &earlier).unwrap()
        };
        assert_eq!(classify(&current), Some(Installed::Current));
        assert_eq!(classify(UNIT_V0_18_1), Some(Installed::Earlier));
        let edited = UNIT_V0_18_1.replace(
            "[Service]\n",
            "[Service]\nEnvironment=HATEL_RETENTION_DAYS=365\n",
        );
        assert_eq!(
            classify(&edited),
            Some(Installed::Other),
            "a hand edit is kept"
        );
        let checkout = UNIT_V0_18_1.replace("/home/u/.local/bin/hatel", "/src/target/debug/hatel");
        assert_eq!(
            classify(&checkout),
            Some(Installed::Other),
            "a unit for another binary keeps pointing where it did"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_systemd_unit_this_build_writes_is_pinned() {
        // A restart replaces only a unit byte-equal to an earlier rendering, so a change to this
        // one also moves it into `earlier_systemd_units`; otherwise an upgrade keeps every
        // installed unit as it is.
        assert_eq!(
            systemd_unit(Path::new("/home/u/.local/bin/hatel")),
            "[Unit]\nDescription=hatel receiver\n\n[Service]\nExecStart=\"/home/u/.local/bin/hatel\" serve --all\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=15\n\n[Install]\nWantedBy=default.target\n"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_systemd_unit_waits_out_a_draining_stop() {
        let unit = systemd_unit(Path::new("/opt/hatel"));
        let stop = STOP_TIMEOUT.as_secs();
        assert!(unit.contains(&format!("\nTimeoutStopSec={stop}\n")));
    }
}
