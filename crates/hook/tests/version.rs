//! `hatel doctor` reads a wired hook's build from exactly this line, so its shape is pinned here.

#[test]
fn version_names_the_build_on_one_line() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_hatel-hook"))
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim_end(),
        format!("hatel-hook {}", env!("CARGO_PKG_VERSION"))
    );
}
