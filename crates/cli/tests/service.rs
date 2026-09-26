//! `service` run as a user would, in a scratch home whose service manager cannot be reached.

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unreachable_manager {
    use std::path::{Path, PathBuf};
    use std::process::Output;

    /// The binary under test by its canonical path, which it is also spawned by: Linux reads its own
    /// path back with symlinks resolved and macOS reads back the path it was spawned by, so this is
    /// the one path both read back and the units they recognize name.
    fn exe() -> PathBuf {
        std::fs::canonicalize(env!("CARGO_BIN_EXE_hatel")).unwrap()
    }

    /// A home holding `config`, and where the receiver's unit would be.
    fn home(tag: &str, config: &str) -> (PathBuf, PathBuf) {
        let home =
            std::env::temp_dir().join(format!("ht-cli-service-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), config).unwrap();
        let unit = if cfg!(target_os = "macos") {
            home.join("Library/LaunchAgents/dev.hatel.plist")
        } else {
            home.join(".config/systemd/user/hatel.service")
        };
        (home, unit)
    }

    /// The unit the releases before this one wrote for `exe`.
    fn earlier_unit(exe: &Path) -> String {
        let exe = exe.display().to_string();
        if cfg!(target_os = "macos") {
            let exe = exe
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.hatel</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>serve</string><string>--all</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
</dict></plist>
"#
            )
        } else {
            let exe = exe
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%");
            format!(
                "[Unit]\nDescription=hatel receiver\n\n[Service]\nExecStart=\"{exe}\" serve --all\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n"
            )
        }
    }

    /// An installed unit an earlier release wrote.
    fn install_earlier(unit: &Path) -> String {
        let earlier = earlier_unit(&exe());
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(unit, &earlier).unwrap();
        earlier
    }

    /// No `launchctl` or `systemctl` on PATH: the command can only answer from what it checks
    /// and writes before it reaches the service manager.
    fn service(home: &Path, args: &[&str]) -> Output {
        std::process::Command::new(exe())
            .arg("service")
            .args(args)
            .env("HOME", home)
            .env("HATEL_CONFIG", home.join("config.toml"))
            .env("PATH", home.join("no-such-dir"))
            .env_remove("HATEL_STATE_DIR")
            .env_remove("HATEL_PLUGINS")
            .output()
            .unwrap()
    }

    fn refused(out: &Output) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "stderr: {stderr}");
        assert!(
            stderr.contains("unknown field `pluginz`"),
            "stderr: {stderr}"
        );
    }

    #[test]
    fn a_restart_brings_a_unit_an_earlier_release_wrote_up_to_this_builds() {
        let (home, unit) = home("upgrade", "");
        install_earlier(&unit);
        let out = service(&home, &["--restart"]);
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let print = service(&home, &["--print"]);
        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            String::from_utf8(print.stdout).unwrap()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_restart_into_a_configuration_the_receiver_rejects_changes_nothing() {
        let (home, unit) = home("restart", "pluginz = []\n");
        let earlier = install_earlier(&unit);
        refused(&service(&home, &["--restart"]));
        assert_eq!(std::fs::read_to_string(&unit).unwrap(), earlier);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn an_install_with_a_configuration_the_receiver_rejects_writes_no_unit() {
        let (home, unit) = home("install", "pluginz = []\n");
        refused(&service(&home, &[]));
        assert!(!unit.exists());
        std::fs::remove_dir_all(&home).ok();
    }
}
