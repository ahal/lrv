#![cfg(unix)]

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DIFF: &str = "diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old
+new
";

#[test]
fn sigterm_removes_discovery_entry() {
    let dir = std::env::temp_dir().join(format!("lrv-sigterm-{}", std::process::id()));
    let servers = dir.join("servers");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.diff"), DIFF).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lrv"))
        .args(["--file", "a.diff", "--no-open", "--port", "0"])
        .current_dir(&dir)
        .env("LRV_SERVER_DIR", &servers)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("LRV_COMMENT_DB", dir.join("comments.db"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_dir(&servers).map_or(0, |entries| entries.count()) == 0 {
        assert!(Instant::now() < deadline, "lrv never advertised itself");
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    assert_eq!(child.wait().unwrap().code(), Some(143));
    assert_eq!(fs::read_dir(&servers).unwrap().count(), 0);
    fs::remove_dir_all(dir).unwrap();
}
