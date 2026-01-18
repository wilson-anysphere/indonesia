use nova_cache::Fingerprint;
use std::path::Path;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct ScopedEnvVar {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl ScopedEnvVar {
    fn unset(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn write_origin_config(config_path: &Path, origin: &str) {
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        config_path,
        format!(
            r#"[remote "origin"]
    url = {origin}
"#
        ),
    )
    .unwrap();
}

#[test]
fn project_hash_uses_git_origin_for_repo_root() {
    let _guard = crate::test_lock();

    let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let _env = ScopedEnvVar::unset("NOVA_PROJECT_ID");

    let temp = tempfile::tempdir().unwrap();
    let origin = "https://example.com/repo.git";

    let repo1 = temp.path().join("repo1");
    std::fs::create_dir_all(repo1.join(".git")).unwrap();
    write_origin_config(&repo1.join(".git").join("config"), origin);

    let repo2 = temp.path().join("repo2");
    std::fs::create_dir_all(repo2.join(".git")).unwrap();
    write_origin_config(&repo2.join(".git").join("config"), origin);

    let fp1 = Fingerprint::for_project_root(&repo1).unwrap();
    let fp2 = Fingerprint::for_project_root(&repo2).unwrap();

    let expected = Fingerprint::from_bytes(format!("git:{origin}").as_bytes());
    assert_eq!(fp1, expected);
    assert_eq!(fp2, expected);
    assert_eq!(fp1, fp2);
}

#[test]
fn project_hash_finds_git_origin_from_parent_directory() {
    let _guard = crate::test_lock();

    let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let _env = ScopedEnvVar::unset("NOVA_PROJECT_ID");

    let temp = tempfile::tempdir().unwrap();
    let origin = "https://example.com/repo.git";

    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(repo_root.join(".git")).unwrap();
    write_origin_config(&repo_root.join(".git").join("config"), origin);

    let nested = repo_root.join("subdir").join("more");
    std::fs::create_dir_all(&nested).unwrap();

    let fp_root = Fingerprint::for_project_root(&repo_root).unwrap();
    let fp_nested = Fingerprint::for_project_root(&nested).unwrap();

    let expected = Fingerprint::from_bytes(format!("git:{origin}").as_bytes());
    assert_eq!(fp_root, expected);
    assert_eq!(fp_nested, expected);
    assert_eq!(fp_root, fp_nested);
}

#[test]
fn project_hash_supports_worktree_gitdir_file() {
    let _guard = crate::test_lock();

    let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let _env = ScopedEnvVar::unset("NOVA_PROJECT_ID");

    let temp = tempfile::tempdir().unwrap();
    let origin = "https://example.com/repo.git";

    let worktree_root = temp.path().join("worktree");
    std::fs::create_dir_all(&worktree_root).unwrap();

    let gitdir = temp.path().join("external-gitdir");
    std::fs::create_dir_all(&gitdir).unwrap();
    write_origin_config(&gitdir.join("config"), origin);

    // `.git` file format used by git worktrees and submodules.
    std::fs::write(worktree_root.join(".git"), "gitdir: ../external-gitdir\n").unwrap();

    let fp = Fingerprint::for_project_root(&worktree_root).unwrap();
    let expected = Fingerprint::from_bytes(format!("git:{origin}").as_bytes());
    assert_eq!(fp, expected);
}

#[test]
fn project_hash_supports_worktree_commondir() {
    let _guard = crate::test_lock();

    let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let _env = ScopedEnvVar::unset("NOVA_PROJECT_ID");

    let temp = tempfile::tempdir().unwrap();
    let origin = "https://example.com/repo.git";

    let worktree_root = temp.path().join("worktree");
    std::fs::create_dir_all(&worktree_root).unwrap();

    let gitdir = temp.path().join("gitdir");
    std::fs::create_dir_all(&gitdir).unwrap();
    // Worktree-local config without remotes.
    std::fs::write(
        gitdir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n",
    )
    .unwrap();

    let common = temp.path().join("common");
    std::fs::create_dir_all(&common).unwrap();
    write_origin_config(&common.join("config"), origin);

    std::fs::write(gitdir.join("commondir"), "../common\n").unwrap();
    std::fs::write(worktree_root.join(".git"), "gitdir: ../gitdir\n").unwrap();

    let fp = Fingerprint::for_project_root(&worktree_root).unwrap();
    let expected = Fingerprint::from_bytes(format!("git:{origin}").as_bytes());
    assert_eq!(fp, expected);
}
