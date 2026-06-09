//! `hatel-hook` — read one Claude Code hook event on stdin, record it,
//! exit 0. This is the command wired into `settings.json` hooks.

use std::io::Read;

fn main() {
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    std::process::exit(hatel_core::hook::run_hook(&buf));
}
