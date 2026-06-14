//! `hatel-hook` — read one Claude Code hook event on stdin, record it,
//! exit 0. This is the command wired into `settings.json` hooks.

use std::io::Read;

/// A hook event is small JSON emitted by Claude Code; cap the read so a pathological or runaway
/// stdin can never drive an unbounded allocation in a process that must not block or OOM the agent.
/// No real event approaches this, so the only effect of the cap is fail-open: an over-large input
/// truncates, fails to parse, and `run_hook` records nothing and exits 0.
const MAX_STDIN_BYTES: u64 = 64 * 1024 * 1024;

fn main() {
    let mut buf = String::new();
    let _ = std::io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_string(&mut buf);
    std::process::exit(hatel_core::hook::run_hook(&buf));
}
