use std::path::{Path, PathBuf};
use std::process::Command;

/// Find the repository root containing `cwd`.
///
/// Prefer jj because a jj-only repository has no `.git` directory. Git is the
/// fallback for ordinary Git repositories.
pub fn root(cwd: &Path) -> Option<PathBuf> {
    command_root("jj", &["root"], cwd)
        .or_else(|| command_root("git", &["rev-parse", "--show-toplevel"], cwd))
}

pub fn is_jj_repo(root: impl AsRef<Path>) -> bool {
    root.as_ref().join(".jj").exists()
}

fn command_root(program: &str, args: &[&str], cwd: &Path) -> Option<PathBuf> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    (!path.as_os_str().is_empty() && path.is_dir()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::{is_jj_repo, root};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[cfg(unix)]
    #[test]
    fn finds_a_jj_only_repository_from_a_subdirectory() {
        use std::os::unix::fs::PermissionsExt;

        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("lrv-repository-test-{}-{id}", std::process::id()));
        let repo = base.join("repo");
        let nested = repo.join("src");
        let bin = base.join("bin");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir(repo.join(".jj")).unwrap();

        let jj = bin.join("jj");
        fs::write(
            &jj,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}'\n",
                repo.to_string_lossy().replace('\'', "'\\''")
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&jj).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&jj, permissions).unwrap();

        let original_path = std::env::var_os("PATH");
        let path = match &original_path {
            Some(path) => format!("{}:{}", bin.display(), path.to_string_lossy()),
            None => bin.display().to_string(),
        };
        std::env::set_var("PATH", path);

        assert_eq!(root(&nested), Some(PathBuf::from(&repo)));
        assert!(is_jj_repo(&repo));

        match original_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        let _ = fs::remove_dir_all(base);
    }
}
