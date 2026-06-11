//! Project identity from a working directory. The native OTel stream carries no
//! project attribute, so the hook layer derives one from `cwd` and the receiver
//! joins on it. The key is the git-root absolute path (unique per repository, so
//! two same-named repos never merge); the label is its basename for display.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    pub key: String,
    pub label: String,
}

pub fn resolve_project(cwd: &str) -> ProjectRef {
    let start = Path::new(cwd);
    let root = git_root(start).unwrap_or_else(|| start.to_path_buf());
    let key = root.to_string_lossy().into_owned();
    // A root without a basename (e.g. `/`) falls back to the full path as its label —
    // still visible and still matchable by a filter entry — rather than an empty string,
    // which would render as a ghost project and could never be listed in a filter.
    let label = root
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| key.clone());
    ProjectRef { key, label }
}

/// Walk up from `start` to the nearest directory containing `.git` (the worktree root).
/// `None` outside a repository.
pub fn git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Current git branch for `cwd`, read from `.git/HEAD` (no subprocess). Handles
/// worktrees and submodules, where `.git` is a file pointing at the real git dir.
/// Returns `None` on a detached HEAD or outside a repo — never a guessed value.
pub fn git_branch(cwd: &str) -> Option<String> {
    let root = git_root(Path::new(cwd))?;
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let rel = content.strip_prefix("gitdir:")?.trim();
        let path = PathBuf::from(rel);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    Some(head.trim().strip_prefix("ref: refs/heads/")?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ht-proj-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn branch_from_a_normal_git_dir_with_crlf() {
        let repo = scratch();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\r\n").unwrap();
        assert_eq!(
            git_branch(repo.to_str().unwrap()).as_deref(),
            Some("feature/x")
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn branch_from_a_worktree_git_file_redirect() {
        // A worktree's `.git` is a FILE redirecting to the real git dir via `gitdir:`.
        let repo = scratch();
        let real = repo.join("realgit");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(repo.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();
        assert_eq!(git_branch(repo.to_str().unwrap()).as_deref(), Some("wt"));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_root_path_labels_as_the_path_not_an_empty_string() {
        // `/` has no basename; the label falls back to the path so the project stays
        // visible in reports and addressable by a filter entry.
        let p = resolve_project("/");
        assert_eq!(p.key, "/");
        assert_eq!(p.label, "/", "no-basename root labels as its path");
        assert!(!p.label.is_empty());
    }

    #[test]
    fn detached_head_and_non_repo_yield_none() {
        let repo = scratch();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "a1b2c3d4e5f6\n").unwrap(); // detached
        assert_eq!(git_branch(repo.to_str().unwrap()), None);
        std::fs::remove_dir_all(&repo).ok();
        assert_eq!(git_branch("/tmp/definitely-not-a-repo-xyz"), None);
    }
}
