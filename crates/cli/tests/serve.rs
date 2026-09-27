//! `serve` against a port or a state dir another process holds, as the service runs it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A home whose config.toml keeps the store in it.
fn home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("ht-cli-serve-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    configure(&home, "");
    home
}

/// Write the home's config.toml: the store in the home, and `storage` under `[storage]`. Written
/// whole and renamed into place, as a receiver waiting meanwhile reads the file on every attempt.
fn configure(home: &Path, storage: &str) {
    let state = home.join("state");
    let staged = home.join("config.toml.tmp");
    std::fs::write(
        &staged,
        format!(
            "[storage]\nstate_dir = {:?}\n{storage}",
            state.display().to_string()
        ),
    )
    .unwrap();
    std::fs::rename(&staged, home.join("config.toml")).unwrap();
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A receiver this test started, killed when dropped so a failed assertion leaves no process
/// behind.
struct Receiver(Child);

impl Drop for Receiver {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn serve(home: &Path, port: u16, wait: bool) -> Receiver {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hatel"));
    // No `HATEL_*` variable of the environment the tests run in reaches the receiver, which
    // configures itself from the home's config.toml alone. Compared without case, as Windows reads
    // the names.
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|k| k.to_ascii_uppercase().starts_with("HATEL_"))
        {
            cmd.env_remove(key);
        }
    }
    cmd.args(["serve", "--all", "--port", &port.to_string()]);
    if wait {
        cmd.arg("--wait");
    }
    cmd.env("HOME", home)
        .env("HATEL_CONFIG", home.join("config.toml"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Receiver(cmd.spawn().unwrap())
}

/// The `/healthz` body a hatel receiver answers on `port`, if one does.
fn healthz(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    let request = "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    (head.starts_with("HTTP/1.1 200") && body.contains("\"service\":\"hatel\""))
        .then(|| body.to_string())
}

fn answers(port: u16) -> bool {
    healthz(port).is_some()
}

fn within(limit: Duration, done: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn exit_within(receiver: &mut Receiver, limit: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(status) = receiver.0.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn still_running(receiver: &mut Receiver) -> bool {
    receiver.0.try_wait().unwrap().is_none()
}

const LIMIT: Duration = Duration::from_secs(10);

#[test]
fn a_receiver_without_wait_exits_when_the_port_is_taken() {
    let home = home("taken");
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut receiver = serve(&home, held.local_addr().unwrap().port(), false);
    let status = exit_within(&mut receiver, LIMIT);
    drop(receiver);
    assert!(status.is_some_and(|s| !s.success()), "{status:?}");
    std::fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn a_waiting_receiver_stops_at_once_when_asked() {
    let home = home("stop");
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut receiver = serve(&home, held.local_addr().unwrap().port(), true);
    std::thread::sleep(Duration::from_millis(1500));
    let pid = libc::pid_t::try_from(receiver.0.id()).unwrap();
    // SAFETY: `kill` on the pid of a child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let status = exit_within(&mut receiver, Duration::from_secs(3));
    drop(receiver);
    assert!(status.is_some_and(|s| s.success()), "{status:?}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn a_waiting_receiver_serves_once_the_port_is_freed() {
    let home = home("port");
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let mut receiver = serve(&home, port, true);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(still_running(&mut receiver), "it waits rather than exits");
    drop(held);
    let served = within(LIMIT, || answers(port));
    drop(receiver);
    assert!(served, "it serves once the port is free");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn a_receiver_waiting_for_the_port_leaves_the_store_to_another() {
    let home = home("share");
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut first = serve(&home, held.local_addr().unwrap().port(), true);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(still_running(&mut first), "it waits for the port");
    // `--wait`, so the instant the first holds the lock on each of its attempts cannot fail it.
    let port = free_port();
    let second = serve(&home, port, true);
    let served = within(LIMIT, || answers(port));
    drop(second);
    drop(first);
    assert!(served, "the store is free for a receiver on another port");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn a_waiting_receiver_takes_over_the_store_once_its_holder_exits() {
    let home = home("lock");
    let first_port = free_port();
    let first = serve(&home, first_port, false);
    assert!(within(LIMIT, || answers(first_port)));
    let second_port = free_port();
    let mut second = serve(&home, second_port, true);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        still_running(&mut second) && !answers(second_port),
        "it waits for the store's lock"
    );
    drop(first);
    let served = within(LIMIT, || answers(second_port));
    drop(second);
    assert!(served, "it takes over once the holder exits");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn a_waiting_receiver_serves_the_configuration_it_finds_when_it_takes_over() {
    let home = home("config");
    let first_port = free_port();
    let first = serve(&home, first_port, false);
    assert!(within(LIMIT, || answers(first_port)));
    let second_port = free_port();
    let mut second = serve(&home, second_port, true);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(still_running(&mut second), "it waits for the store's lock");
    configure(&home, "retention_days = 45\n");
    drop(first);
    let current = within(LIMIT, || {
        healthz(second_port).is_some_and(|body| body.contains("\"retention_days\":45"))
    });
    drop(second);
    assert!(current, "it reads config.toml again when it takes over");
    std::fs::remove_dir_all(&home).ok();
}
