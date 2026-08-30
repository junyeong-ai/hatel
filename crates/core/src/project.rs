//! Project identity from a working directory. The native OTel stream carries no
//! project attribute, so the hook layer derives one from `cwd` and the receiver
//! joins on it. The key is the repository's main working tree as an absolute path
//! (unique per repository, so two same-named repos never merge); the label is its
//! basename for display.
//!
//! A repository is what makes a project nameable — a durable unit of work that
//! outlives the session and means the same thing to everyone. A directory is only
//! where one was found, so work outside a repository has no project rather than one
//! named after whatever directory it ran in.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    pub key: String,
    pub label: String,
}

/// The project the working directory `cwd` belongs to, or `None` where there is no project to
/// name — outside a repository, or where `cwd` names no tree this process can resolve.
pub fn resolve_project(cwd: &str) -> Option<ProjectRef> {
    // A linked worktree is a checkout of a repository, not a project of its own — work done on a
    // branch in one belongs to the repository's totals, not to a project named after the branch
    // it happened to be checked out for.
    let tree = git_root(Path::new(cwd))?;
    let root = main_worktree(&tree).unwrap_or(tree);
    let key = root.to_string_lossy().into_owned();
    // A root without a basename (a repository at `/`) falls back to the full path as its label —
    // still visible and still matchable by a filter entry — rather than an empty string, which
    // would render as a ghost project and could never be listed in a filter.
    let label = root
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| key.clone());
    Some(ProjectRef { key, label })
}

/// Walk up from `start` to the nearest working tree root (the directory holding `.git`).
/// `None` outside a repository, and for a `start` that is not absolute: a relative path is
/// resolved against the calling process's own working directory, which for a hook is wherever it
/// was spawned rather than anywhere its session is — so every answer derived from one would
/// describe the wrong tree. This is the *containing* tree, so a linked worktree resolves to
/// itself — what anything anchored at the checkout (its `.claude`, its `HEAD`) needs.
pub fn git_root(start: &Path) -> Option<PathBuf> {
    if !start.is_absolute() {
        return None;
    }
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Current git branch for `cwd`, read from the containing working tree's own `HEAD` (no
/// subprocess), so a linked worktree reports the branch it is on rather than the main
/// checkout's. `None` on a detached HEAD or outside a repo — never a guessed value.
pub fn git_branch(cwd: &str) -> Option<String> {
    let git_dir = git_dir(&git_root(Path::new(cwd))?)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    Some(head.trim().strip_prefix("ref: refs/heads/")?.to_string())
}

/// The git directory serving the working tree at `root`: `.git` itself, or — where `.git` is a
/// file, as in a linked worktree or a submodule — the directory it redirects to.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let redirect = std::fs::read_to_string(&dot_git).ok()?;
    Some(recorded_path(
        root,
        redirect.strip_prefix("gitdir:")?.trim(),
    ))
}

/// The main working tree of the repository the tree at `root` belongs to, or `None` when the
/// repository does not name one. `commondir` is written only for a linked worktree, and names
/// the git directory it shares with the rest of the repository; a working tree's git directory
/// is the `.git` inside it, so the checkout is its parent. A submodule records no `commondir`,
/// and a bare repository — or a git directory held outside a checkout, as a submodule's or a
/// `--separate-git-dir` repository's is — shares a directory that names no checkout. Each keeps
/// the identity of the tree it was resolved from, which only ever costs the collapse, never
/// attributing a tree to a repository that is not its own.
///
/// The shared directory is resolved through the filesystem rather than by folding `..` away in
/// this process: the checkout is named by the path's own last component, and only the kernel can
/// say which directory a `..` leaves when it crosses a symlink.
fn main_worktree(root: &Path) -> Option<PathBuf> {
    let git_dir = git_dir(root)?;
    let recorded = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common = std::fs::canonicalize(recorded_path(&git_dir, recorded.trim())).ok()?;
    match common.file_name() {
        Some(name) if name == ".git" => common.parent().map(Path::to_path_buf),
        _ => None,
    }
}

/// A path git records inside its own directory, written as either absolute or relative to the
/// directory holding it.
fn recorded_path(base: &Path, recorded: &str) -> PathBuf {
    let path = Path::new(recorded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
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

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// The project for a tree these tests expect to resolve to one.
    fn project(cwd: &Path) -> ProjectRef {
        resolve_project(cwd.to_str().unwrap()).expect("a tree in a repository has a project")
    }

    /// A repository with one linked worktree, laid out as `git worktree add` writes it: the
    /// checkout's `.git` is a file naming its private git directory, which records the shared
    /// one relatively. Returns the repository root and the checkout.
    fn repo_with_worktree(base: &Path) -> (PathBuf, PathBuf) {
        let repo = base.join("acme-api");
        let checkout = repo.join(".worktrees/spec-x");
        write(&repo.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(
            &repo.join(".git/worktrees/spec-x/HEAD"),
            "ref: refs/heads/spec/x\n",
        );
        write(&repo.join(".git/worktrees/spec-x/commondir"), "../..\n");
        write(
            &checkout.join(".git"),
            &format!("gitdir: {}\n", repo.join(".git/worktrees/spec-x").display()),
        );
        (repo, checkout)
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
    fn a_tree_outside_a_repository_has_no_project() {
        // A directory is where a project was found, not one itself. Naming a project after
        // whatever directory the work ran in invents a home directory, a container of
        // repositories, and every scratch directory as projects of their own.
        let dir = scratch();
        assert!(resolve_project(dir.to_str().unwrap()).is_none());
        assert!(resolve_project("/").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_cwd_that_is_not_absolute_names_no_tree() {
        // A relative path resolves against whatever directory this process was started in, which
        // has nothing to do with the session. `../..` is this crate's own repository root when the
        // tests run there, so without the guard both answers would describe the tree the test
        // runner happens to sit in — every dimension is absent instead of borrowed.
        for cwd in ["", ".", "../.."] {
            assert!(resolve_project(cwd).is_none(), "project for {cwd:?}");
            assert!(git_branch(cwd).is_none(), "branch for {cwd:?}");
        }
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

    #[test]
    fn a_linked_worktree_attributes_to_its_repository() {
        // Otherwise every spec branch checked out into its own tree becomes a project named
        // after that branch, and the repository's own totals silently lose that work.
        let base = scratch();
        let (repo, checkout) = repo_with_worktree(&base);
        let deep = checkout.join("crates/core/src");
        std::fs::create_dir_all(&deep).unwrap();

        let repo = std::fs::canonicalize(&repo).unwrap();
        for from in [&checkout, &deep] {
            let p = project(from);
            assert_eq!(Path::new(&p.key), repo, "from {}", from.display());
            assert_eq!(p.label, "acme-api", "from {}", from.display());
        }
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_worktree_reports_the_branch_it_is_on_not_the_repositorys() {
        // Project attribution collapses a worktree into its repository; the branch must not,
        // or a `spec/<slug>` binding would capture the main checkout's branch instead.
        let base = scratch();
        let (repo, checkout) = repo_with_worktree(&base);
        assert_eq!(
            git_branch(checkout.to_str().unwrap()).as_deref(),
            Some("spec/x")
        );
        assert_eq!(git_branch(repo.to_str().unwrap()).as_deref(), Some("main"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_checkout_reached_through_a_symlink_resolves_through_the_filesystem() {
        // Git writes a submodule's redirect relative to the checkout, so where the checkout is
        // reached through a symlink only the kernel can say what `..` leaves. Folding it away in
        // process would name a directory that does not exist and lose the branch entirely.
        let base = scratch();
        let sup = base.join("super");
        write(&sup.join(".git/modules/lib/HEAD"), "ref: refs/heads/dev\n");
        write(&sup.join("lib/.git"), "gitdir: ../.git/modules/lib\n");
        let alias = base.join("lib-alias");
        std::os::unix::fs::symlink(sup.join("lib"), &alias).unwrap();

        assert_eq!(git_branch(alias.to_str().unwrap()).as_deref(), Some("dev"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_worktree_recorded_relatively_still_attributes_to_its_repository() {
        // `worktree.useRelativePaths` records the redirect relative to the checkout too, so the
        // repository is only reachable by letting the filesystem cross the symlink.
        let base = scratch();
        let repo = base.join("repo");
        let checkout = base.join("trees/wt");
        write(&repo.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(
            &repo.join(".git/worktrees/wt/HEAD"),
            "ref: refs/heads/spec/y\n",
        );
        write(&repo.join(".git/worktrees/wt/commondir"), "../..\n");
        write(
            &checkout.join(".git"),
            "gitdir: ../../repo/.git/worktrees/wt\n",
        );
        let alias = base.join("wt-alias");
        std::os::unix::fs::symlink(&checkout, &alias).unwrap();

        let p = project(&alias);
        assert_eq!(p.label, "repo");
        assert_eq!(
            Path::new(&p.key),
            std::fs::canonicalize(&repo).unwrap(),
            "attributes to the repository the worktree belongs to"
        );
        assert_eq!(
            git_branch(alias.to_str().unwrap()).as_deref(),
            Some("spec/y")
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_worktree_of_a_submodule_keeps_its_own_identity() {
        // Its shared directory is the submodule's `.git/modules/<name>`, which names no checkout;
        // the repository is recorded only in that directory's config, so the tree keeps its own
        // identity rather than being attributed by a guess.
        let base = scratch();
        let module = base.join("super/.git/modules/lib");
        let checkout = base.join("subwt");
        write(&module.join("HEAD"), "ref: refs/heads/main\n");
        write(&module.join("worktrees/w/HEAD"), "ref: refs/heads/sub-wt\n");
        write(&module.join("worktrees/w/commondir"), "../..\n");
        write(
            &checkout.join(".git"),
            &format!("gitdir: {}\n", module.join("worktrees/w").display()),
        );

        let p = project(&checkout);
        assert_eq!(p.key, checkout.to_string_lossy());
        assert_eq!(p.label, "subwt");
        assert_eq!(
            git_branch(checkout.to_str().unwrap()).as_deref(),
            Some("sub-wt")
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_submodule_is_its_own_project() {
        // A submodule also reaches its git directory through a `.git` file — a relative one —
        // but it is a repository in its own right, not a checkout of its superproject.
        let base = scratch();
        let sup = base.join("super");
        write(&sup.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&sup.join(".git/modules/lib/HEAD"), "ref: refs/heads/dev\n");
        write(&sup.join("lib/.git"), "gitdir: ../.git/modules/lib\n");

        let p = project(&sup.join("lib"));
        assert_eq!(p.key, sup.join("lib").to_string_lossy());
        assert_eq!(p.label, "lib");
        assert_eq!(
            git_branch(sup.join("lib").to_str().unwrap()).as_deref(),
            Some("dev"),
            "a relative gitdir still resolves"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_shared_git_dir_outside_a_checkout_attributes_to_no_repository() {
        // A bare repository's worktree (and equally a submodule's) shares a directory that is
        // not some checkout's `.git`, so there is no repository root to attribute to; the tree
        // keeps its own identity rather than borrowing the shared directory's name.
        let base = scratch();
        let checkout = base.join("co");
        write(
            &base.join("bare.git/worktrees/wt/HEAD"),
            "ref: refs/heads/wt\n",
        );
        write(
            &base.join("bare.git/worktrees/wt/commondir"),
            &format!("{}\n", base.join("bare.git").display()),
        );
        write(
            &checkout.join(".git"),
            &format!("gitdir: {}\n", base.join("bare.git/worktrees/wt").display()),
        );

        let p = project(&checkout);
        assert_eq!(p.key, checkout.to_string_lossy());
        assert_eq!(p.label, "co");
        std::fs::remove_dir_all(&base).ok();
    }
}
