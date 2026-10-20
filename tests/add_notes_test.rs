use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn add_notes_without_running_review() {
    let dir = std::env::temp_dir().join(format!("lrv-add-notes-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lrv"))
        .args(["--add-notes", "-"])
        .env("LRV_SERVER_DIR", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"[]").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("No running lrv found for"));
    assert!(output.stdout.is_empty());
    fs::remove_dir_all(dir).unwrap();
}
