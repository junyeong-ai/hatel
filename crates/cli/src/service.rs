//! `service` — install or remove the receiver as a per-user background service so native OTel is
//! captured gap-free, not only while `serve` runs in a terminal. macOS uses a launchd LaunchAgent,
//! Linux a systemd `--user` unit; the unit runs `serve --all --wait` from this exact binary. Like `init`,
//! the binary owns this OS integration (rather than a copy-pasted plist/unit), so it is consistent,
//! idempotent, and `--print`-able for managed or customized setups. `--restart` is what an
//! upgrade needs: a running receiver keeps the binary it started from, so the installer restarts
//! the service — and only if one is installed and running, never installing one as a side effect.
//! That restart also brings a unit an earlier release wrote for this binary up to this build's, so
//! a change to the unit reaches an upgraded install; a unit edited by hand or written for another
//! binary is kept. Installing points a unit hatel wrote for another binary at this one, and keeps
//! one edited by hand, since replacing it would silently drop what it sets. The unit carries
//! nothing to configure: the receiver reads its settings from `config.toml`, as every other
//! process does. Other platforms are reported honestly as
//! unsupported — run `serve --all` under your own supervisor.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::Write as _;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::{Path, PathBuf};
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

/// How long launchd waits to start the receiver again after a start that exited. launchd measures
/// it from that start, so a receiver that ran longer is started again at once; one that cannot
/// start (a broken config.toml) writes a line to its unrotated log per interval rather than one
/// every 10 s, launchd's default. A port or lock another receiver holds is waited for instead
/// (`serve --wait`), so taking over from it does not wait out this interval.
#[cfg(target_os = "macos")]
const RELAUNCH_THROTTLE: std::time::Duration = std::time::Duration::from_secs(300);

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
    let installed = match installed_unit(
        &path,
        exe,
        |exe| launchd_plists(&label, exe, &log),
        xml_unescape,
    ) {
        Ok(installed) => installed,
        Err(e) => {
            eprintln!("service: {e}");
            return 1;
        }
    };
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let target = format!("gui/{}/{label}", unsafe { libc::getuid() });
    if action == Action::Restart {
        let Some(installed) = installed else {
            println!("no receiver service installed");
            return 0;
        };
        if !receiver_would_start() {
            return 1;
        }
        if let Some(note) = kept_unit(&path, &installed) {
            println!("{note}");
        }
        if installed == Installed::Earlier
            && let Err(e) = write_launchd_unit(&path, &plist, &log)
        {
            eprintln!("service: {e}");
            return 1;
        }
        return restart_agent(&target, &path, &installed);
    }

    // A unit edited by hand is kept, and restarted as it is, so an upgrade that installs still
    // moves its receiver to the binary now on disk; the install itself did not happen.
    if installed == Some(Installed::Other) {
        eprintln!("service: {}", hand_edited(&path));
        if receiver_would_start() {
            restart_agent(&target, &path, &Installed::Other);
        }
        return 1;
    }
    if !receiver_would_start() {
        return 1;
    }
    match write_launchd_unit(&path, &plist, &log).and_then(|()| reload(&target, &path)) {
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

/// The LaunchAgent that runs `serve --all --wait` from `exe`. launchd discards a job's output unless the
/// plist names a file, so the receiver's stdout and stderr go to `log`; it waits [`STOP_TIMEOUT`]
/// after SIGTERM, not its own shorter default, so a stop can finish draining egress; and it starts
/// a receiver that exited early again only after [`RELAUNCH_THROTTLE`].
#[cfg(target_os = "macos")]
fn launchd_plist(label: &str, exe: &Path, log: &Path) -> String {
    let exe_xml = xml_escape(&exe.display().to_string());
    let log_xml = xml_escape(&log.display().to_string());
    let stop = STOP_TIMEOUT.as_secs();
    let throttle = RELAUNCH_THROTTLE.as_secs();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe_xml}</string><string>serve</string><string>--all</string><string>--wait</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>{throttle}</integer>
  <key>ExitTimeOut</key><integer>{stop}</integer>
  <key>StandardOutPath</key><string>{log_xml}</string>
  <key>StandardErrorPath</key><string>{log_xml}</string>
</dict></plist>
"#
    )
}

/// Every plist hatel writes for `exe`: this build's, then the earlier releases'.
#[cfg(target_os = "macos")]
fn launchd_plists(label: &str, exe: &Path, log: &Path) -> Vec<String> {
    let mut plists = vec![launchd_plist(label, exe, log)];
    plists.extend(earlier_launchd_plists(label, exe, log));
    plists
}

/// The plists earlier releases wrote for `exe` (v0.1.0 to v0.3.0, v0.4.0 to v0.18.1, then v0.19.0):
/// one still byte-equal to them is hatel's own and unedited. Changing [`launchd_plist`] adds the rendering it
/// replaces here, which `the_plist_this_build_writes_is_pinned` enforces.
#[cfg(target_os = "macos")]
fn earlier_launchd_plists(label: &str, exe: &Path, log: &Path) -> Vec<String> {
    let exe_xml = xml_escape(&exe.display().to_string());
    let log_xml = xml_escape(&log.display().to_string());
    vec![
        format!(
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
        ),
        format!(
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
        ),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe_xml}</string><string>serve</string><string>--all</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ExitTimeOut</key><integer>15</integer>
  <key>StandardOutPath</key><string>{log_xml}</string>
  <key>StandardErrorPath</key><string>{log_xml}</string>
</dict></plist>
"#
        ),
    ]
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

/// Restart the loaded agent `target`, or say how to load one that is not loaded: `print` answers
/// only for a loaded service, and one installed but not loaded was stopped on purpose, which a
/// restart does not undo.
#[cfg(target_os = "macos")]
fn restart_agent(target: &str, path: &Path, installed: &Installed) -> i32 {
    if !succeeds(Command::new("launchctl").args(["print", target])) {
        if *installed == Installed::Earlier {
            println!("updated the service unit to this build's");
        }
        if installed.kept() {
            println!(
                "the receiver service is installed but not loaded; `launchctl load -w {}` loads \
                 it as it is",
                path.display()
            );
        } else {
            println!("the receiver service is installed but not loaded; `hatel service` loads it");
        }
        return 0;
    }
    // A plist hatel wrote is loaded again: launchd reads a plist only when it loads the job, and
    // holds a `kickstart` of a job that failed to start until RELAUNCH_THROTTLE has passed, while a
    // job loaded again starts at once. One edited by hand is restarted as launchd loaded it, since
    // hatel cannot vouch for its file.
    let restarted = if *installed == Installed::Other {
        kickstart(target)
    } else {
        reload(target, path)
    };
    match restarted {
        Ok(()) => {
            let done = if *installed == Installed::Earlier {
                "updated the service unit to this build's and restarted the receiver"
            } else {
                "restarted the receiver service"
            };
            println!("{done}; `hatel doctor` says which build answers");
            0
        }
        Err(e) => {
            eprintln!("service: {e}");
            1
        }
    }
}

#[cfg(target_os = "macos")]
fn kickstart(target: &str) -> Result<(), String> {
    match Command::new("launchctl")
        .args(["kickstart", "-k", target])
        .status()
    {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("`launchctl kickstart -k {target}` exited {s}")),
        Err(e) => Err(format!("launchctl not found ({e})")),
    }
}

/// Unload the agent `target` if loaded (a stop, on SIGTERM) and load its plist at `path` again,
/// which starts the receiver from the plist on disk. `unload` returns once the receiver has
/// exited, so two never run at once; `load` exits 0 even when it loads nothing, so the job is
/// looked up afterwards.
#[cfg(target_os = "macos")]
fn reload(target: &str, path: &Path) -> Result<(), String> {
    quiet(Command::new("launchctl").arg("unload").arg(path));
    match Command::new("launchctl")
        .arg("load")
        .arg("-w")
        .arg(path)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            return Err(format!(
                "`launchctl load` exited {s}; the plist is at {}",
                path.display()
            ));
        }
        Err(e) => {
            return Err(format!(
                "launchctl not found ({e}); the plist is at {}",
                path.display()
            ));
        }
    }
    if succeeds(Command::new("launchctl").args(["print", target])) {
        Ok(())
    } else {
        Err(format!(
            "launchd did not load {}; `launchctl load -w` on it says why",
            path.display()
        ))
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
    let installed = match installed_unit(&path, exe, systemd_units, systemd_unescape) {
        Ok(installed) => installed,
        Err(e) => {
            eprintln!("service: {e}");
            return 1;
        }
    };
    if action == Action::Restart {
        let Some(installed) = installed else {
            println!("no receiver service installed");
            return 0;
        };
        if !receiver_would_start() {
            return 1;
        }
        if let Some(note) = kept_unit(&path, &installed) {
            println!("{note}");
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
        return restart_unit(&svc, &installed, reloaded);
    }

    // A unit edited by hand is kept, and restarted as it is, so an upgrade that installs still
    // moves its receiver to the binary now on disk; the install itself did not happen.
    if installed == Some(Installed::Other) {
        eprintln!("service: {}", hand_edited(&path));
        if receiver_would_start() {
            restart_unit(&svc, &Installed::Other, Ok(()));
        }
        return 1;
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

/// The systemd user unit that runs `serve --all --wait` from `exe`, waiting
/// [`STOP_TIMEOUT`] after SIGTERM so a stop can finish draining egress.
#[cfg(target_os = "linux")]
fn systemd_unit(exe: &Path) -> String {
    let exec = systemd_exec_arg(&exe.display().to_string());
    let stop = STOP_TIMEOUT.as_secs();
    format!(
        "[Unit]\nDescription={SERVICE_NAME} receiver\n\n[Service]\nExecStart={exec} serve --all --wait\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec={stop}\n\n[Install]\nWantedBy=default.target\n"
    )
}

/// Every unit hatel writes for `exe`: this build's, then the earlier releases'.
#[cfg(target_os = "linux")]
fn systemd_units(exe: &Path) -> Vec<String> {
    let mut units = vec![systemd_unit(exe)];
    units.extend(earlier_systemd_units(exe));
    units
}

/// The units earlier releases wrote for `exe` (v0.1.0 to v0.3.0, v0.4.0 to v0.18.1, then v0.19.0):
/// one still byte-equal to them is hatel's own and unedited. Changing [`systemd_unit`] adds the rendering it
/// replaces here, which `the_systemd_unit_this_build_writes_is_pinned` enforces.
#[cfg(target_os = "linux")]
fn earlier_systemd_units(exe: &Path) -> Vec<String> {
    let exec = systemd_exec_arg(&exe.display().to_string());
    vec![
        format!(
            "[Unit]\nDescription={SERVICE_NAME} receiver\n\n[Service]\nExecStart={exec} serve --all\nRestart=always\n\n[Install]\nWantedBy=default.target\n"
        ),
        format!(
            "[Unit]\nDescription={SERVICE_NAME} receiver\n\n[Service]\nExecStart={exec} serve --all\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n"
        ),
        format!(
            "[Unit]\nDescription={SERVICE_NAME} receiver\n\n[Service]\nExecStart={exec} serve --all\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=15\n\n[Install]\nWantedBy=default.target\n"
        ),
    ]
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

/// Restart the active unit `svc` once systemd has `reloaded` its file, or say how to start one that
/// is not active: one not active was stopped on purpose, or its manager is out of reach from here
/// (no session bus), and a restart is not the place to start one or to fail an install.
#[cfg(target_os = "linux")]
fn restart_unit(svc: &str, installed: &Installed, reloaded: Result<(), String>) -> i32 {
    if !succeeds(Command::new("systemctl").args(["--user", "is-active", "--quiet", svc])) {
        let start = if installed.kept() {
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
    match Command::new("systemctl")
        .args(["--user", "try-restart", svc])
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
    }
}

/// How an installed unit relates to the one this build writes.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum Installed {
    /// Byte-equal to this build's.
    Current,
    /// Byte-equal to what an earlier release wrote for this binary: hatel's own and unedited, so
    /// replacing it loses nothing.
    Earlier,
    /// What this build or an earlier release wrote for the binary it names. `hatel service` points
    /// it at this binary, which is how a moved install is repointed; a restart from this binary
    /// keeps it, since a restart run from a development checkout must not move the service there.
    AnotherBinary(PathBuf),
    /// Anything else, a hand edit above all. Replacing it would drop whatever it sets, so it is
    /// kept.
    Other,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Installed {
    /// A unit a restart leaves as it is.
    fn kept(&self) -> bool {
        matches!(self, Installed::AnotherBinary(_) | Installed::Other)
    }
}

/// Classify the unit at `path` for `exe`: `renderings` gives every unit hatel writes for a binary,
/// this build's first, and `unescape` inverts how they spell its path. `None` when no unit is
/// installed.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn installed_unit(
    path: &Path,
    exe: &Path,
    renderings: impl Fn(&Path) -> Vec<String>,
    unescape: fn(&str) -> String,
) -> Result<Option<Installed>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let Ok(unit) = String::from_utf8(bytes) else {
        return Ok(Some(Installed::Other));
    };
    Ok(Some(
        match renderings(exe).iter().position(|r| *r == unit) {
            Some(0) => Installed::Current,
            Some(_) => Installed::Earlier,
            None => rendered_binary(&unit, &renderings, unescape)
                .map_or(Installed::Other, Installed::AnotherBinary),
        },
    ))
}

/// The binary `unit` runs when it is exactly one of `renderings` for some binary. Rendering a path
/// no real one can contain, NUL, marks where each rendering spells the binary; `unit` must match
/// the text around that mark, and the path between, unescaped, must render `unit` again byte for
/// byte, so an edit anywhere, the path included, matches no rendering.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rendered_binary(
    unit: &str,
    renderings: &impl Fn(&Path) -> Vec<String>,
    unescape: fn(&str) -> String,
) -> Option<PathBuf> {
    renderings(Path::new("\0")).iter().find_map(|template| {
        let (head, tail) = template.split_once('\0')?;
        let spelled = unit.strip_prefix(head)?.strip_suffix(tail)?;
        let exe = PathBuf::from(unescape(spelled));
        renderings(&exe).iter().any(|r| r == unit).then_some(exe)
    })
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

/// Why a restart keeps the unit at `path`, and how to replace it; `None` for one it replaces.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn kept_unit(path: &Path, installed: &Installed) -> Option<String> {
    match installed {
        Installed::Current | Installed::Earlier => None,
        Installed::AnotherBinary(runs) => Some(format!(
            "kept the installed unit {}, which runs {} rather than this binary; `hatel service` \
             points it at this one",
            path.display(),
            runs.display()
        )),
        Installed::Other => Some(hand_edited(path)),
    }
}

/// Why the unit at `path`, which no release wrote, is kept, and how to replace it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn hand_edited(path: &Path) -> String {
    format!(
        "kept the installed unit {}, which is not one this build or an earlier release wrote. This \
         build's configures nothing: to switch to it, move what this one sets into the default \
         config.toml (HATEL_PLUGINS as `plugins`, storage variables under [storage], a HATEL_CONFIG \
         file's contents into that file), then run `hatel service --remove` and `hatel service` \
         (`hatel service --print` shows this build's unit)",
        path.display()
    )
}

/// Escape a string for XML element text — the binary path goes inside `<string>…</string>`, so a
/// path containing `&`, `<`, or `>` must be escaped to stay valid (and uninjectable).
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Invert [`xml_escape`].
#[cfg(target_os = "macos")]
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
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

/// Invert the escaping [`systemd_exec_arg`] applies inside its quotes.
#[cfg(target_os = "linux")]
fn systemd_unescape(s: &str) -> String {
    let mut path = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' | '%' => path.extend(chars.next()),
            c => path.push(c),
        }
    }
    path
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

    #[cfg(target_os = "macos")]
    #[test]
    fn the_launchd_agent_starts_a_receiver_that_exited_early_only_after_the_throttle() {
        let plist = launchd_plist(
            "dev.hatel",
            Path::new("/opt/hatel"),
            Path::new("/Users/u/Library/Logs/hatel/serve.log"),
        );
        let throttle = RELAUNCH_THROTTLE.as_secs();
        assert!(plist.contains(&format!(
            "<key>ThrottleInterval</key><integer>{throttle}</integer>"
        )));
    }

    /// What v0.1.0 to v0.3.0 wrote as `/Users/u/.local/bin/hatel`'s agent.
    #[cfg(target_os = "macos")]
    const PLIST_V0_3_0: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.hatel</string>
  <key>ProgramArguments</key>
  <array><string>/Users/u/.local/bin/hatel</string><string>serve</string><string>--all</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
"#;

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

    /// What v0.19.0 wrote as `/Users/u/.local/bin/hatel`'s agent.
    #[cfg(target_os = "macos")]
    const PLIST_V0_19_0: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
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
"#;

    #[cfg(target_os = "macos")]
    #[test]
    fn a_plist_is_classified_by_what_wrote_it_and_for_which_binary() {
        let exe = Path::new("/Users/u/.local/bin/hatel");
        let log = Path::new("/Users/u/Library/Logs/hatel/serve.log");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev.hatel.plist");
        let classify = |text: &str| {
            std::fs::write(&path, text).unwrap();
            installed_unit(
                &path,
                exe,
                |exe| launchd_plists("dev.hatel", exe, log),
                xml_unescape,
            )
            .unwrap()
        };
        assert_eq!(
            classify(&launchd_plist("dev.hatel", exe, log)),
            Some(Installed::Current)
        );
        assert_eq!(classify(PLIST_V0_3_0), Some(Installed::Earlier));
        assert_eq!(classify(PLIST_V0_18_1), Some(Installed::Earlier));
        assert_eq!(classify(PLIST_V0_19_0), Some(Installed::Earlier));
        assert_eq!(
            classify(&PLIST_V0_18_1.replace(exe.to_str().unwrap(), "/src/target/debug/hatel")),
            Some(Installed::AnotherBinary("/src/target/debug/hatel".into())),
        );
        assert_eq!(
            classify(&launchd_plist(
                "dev.hatel",
                Path::new("/opt/a&b/hatel"),
                log
            )),
            Some(Installed::AnotherBinary("/opt/a&b/hatel".into())),
        );
        let environment = PLIST_V0_18_1.replace(
            "  <key>RunAtLoad</key>",
            "  <key>EnvironmentVariables</key><dict><key>HATEL_RETENTION_DAYS</key>\
             <string>365</string></dict>\n  <key>RunAtLoad</key>",
        );
        assert_eq!(classify(&environment), Some(Installed::Other));
        let argument = PLIST_V0_18_1.replace(
            "/hatel</string>",
            "/hatel</string><string>--verbose</string>",
        );
        assert_eq!(
            classify(&argument),
            Some(Installed::Other),
            "an edit next to the binary's path is no rendering of another path"
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            installed_unit(
                &path,
                exe,
                |exe| launchd_plists("dev.hatel", exe, log),
                xml_unescape
            )
            .unwrap(),
            None
        );
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
  <array><string>/Users/u/.local/bin/hatel</string><string>serve</string><string>--all</string><string>--wait</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>300</integer>
  <key>ExitTimeOut</key><integer>15</integer>
  <key>StandardOutPath</key><string>/Users/u/Library/Logs/hatel/serve.log</string>
  <key>StandardErrorPath</key><string>/Users/u/Library/Logs/hatel/serve.log</string>
</dict></plist>
"#
        );
    }

    /// What v0.1.0 to v0.3.0 wrote as `/home/u/.local/bin/hatel`'s unit.
    #[cfg(target_os = "linux")]
    const UNIT_V0_3_0: &str = "[Unit]\nDescription=hatel receiver\n\n[Service]\nExecStart=\"/home/u/.local/bin/hatel\" serve --all\nRestart=always\n\n[Install]\nWantedBy=default.target\n";

    /// What v0.19.0 wrote as `/home/u/.local/bin/hatel`'s unit.
    #[cfg(target_os = "linux")]
    const UNIT_V0_19_0: &str = "[Unit]\nDescription=hatel receiver\n\n[Service]\nExecStart=\"/home/u/.local/bin/hatel\" serve --all\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=15\n\n[Install]\nWantedBy=default.target\n";

    /// What v0.18.1 wrote as `/home/u/.local/bin/hatel`'s unit.
    #[cfg(target_os = "linux")]
    const UNIT_V0_18_1: &str = "[Unit]\nDescription=hatel receiver\n\n[Service]\nExecStart=\"/home/u/.local/bin/hatel\" serve --all\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n";

    #[cfg(target_os = "linux")]
    #[test]
    fn a_unit_is_classified_by_what_wrote_it_and_for_which_binary() {
        let exe = Path::new("/home/u/.local/bin/hatel");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hatel.service");
        let classify = |text: &str| {
            std::fs::write(&path, text).unwrap();
            installed_unit(&path, exe, systemd_units, systemd_unescape).unwrap()
        };
        assert_eq!(classify(&systemd_unit(exe)), Some(Installed::Current));
        assert_eq!(classify(UNIT_V0_3_0), Some(Installed::Earlier));
        assert_eq!(classify(UNIT_V0_18_1), Some(Installed::Earlier));
        assert_eq!(classify(UNIT_V0_19_0), Some(Installed::Earlier));
        assert_eq!(
            classify(&UNIT_V0_18_1.replace(exe.to_str().unwrap(), "/src/target/debug/hatel")),
            Some(Installed::AnotherBinary("/src/target/debug/hatel".into())),
        );
        assert_eq!(
            classify(&systemd_unit(Path::new("/opt/100%/ha\"t\\el"))),
            Some(Installed::AnotherBinary("/opt/100%/ha\"t\\el".into())),
        );
        let environment = UNIT_V0_18_1.replace(
            "[Service]\n",
            "[Service]\nEnvironment=HATEL_RETENTION_DAYS=365\n",
        );
        assert_eq!(classify(&environment), Some(Installed::Other));
        let argument = UNIT_V0_18_1.replace("hatel\" serve", "hatel\" \"--verbose\" serve");
        assert_eq!(
            classify(&argument),
            Some(Installed::Other),
            "an edit next to the binary's path is no rendering of another path"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_systemd_unit_this_build_writes_is_pinned() {
        // A restart replaces only a unit byte-equal to an earlier rendering, so a change to this
        // one also moves it into `earlier_systemd_units`; otherwise an upgrade keeps every
        // installed unit as it is.
        assert_eq!(
            systemd_unit(Path::new("/home/u/.local/bin/hatel")),
            "[Unit]\nDescription=hatel receiver\n\n[Service]\nExecStart=\"/home/u/.local/bin/hatel\" serve --all --wait\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=15\n\n[Install]\nWantedBy=default.target\n"
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
