use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

fn git(root: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git in {}", root.display()))
}

fn checked(root: &Path, args: &[&str]) -> Result<String> {
    let output = git(root, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn repository_root(path: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(checked(
        path,
        &["rev-parse", "--show-toplevel"],
    )?))
}

pub fn current_branch(root: &Path) -> Result<String> {
    let branch = checked(root, &["branch", "--show-current"])?;
    if branch.is_empty() {
        bail!("Cadence requires a checked-out branch, not detached HEAD");
    }
    Ok(branch)
}

pub fn head(root: &Path) -> Result<String> {
    checked(root, &["rev-parse", "HEAD"])
}

pub fn resolve_commit(root: &Path, revision: &str) -> Result<String> {
    let commit = format!("{revision}^{{commit}}");
    checked(
        root,
        &["rev-parse", "--verify", "--end-of-options", &commit],
    )
}

pub fn ensure_clean(root: &Path) -> Result<()> {
    if !is_clean(root)? {
        bail!("Git worktree is dirty; commit or stash changes before continuing");
    }
    Ok(())
}

pub fn is_clean(root: &Path) -> Result<bool> {
    Ok(checked(root, &["status", "--porcelain"])?.is_empty())
}

pub fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git(root, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

pub fn commits_between(root: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let range = format!("{base}..{head}");
    let output = checked(root, &["rev-list", "--reverse", &range])?;
    Ok(output
        .lines()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect())
}

pub fn changed_paths(root: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let range = format!("{base}..{head}");
    let output = git(root, &["diff", "--name-only", "-z", &range])?;
    if !output.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec())
                .context("Cadence cannot integrate a non-UTF-8 Git path")
        })
        .collect()
}

pub fn changed_paths_for_commit(root: &Path, commit: &str) -> Result<Vec<String>> {
    changed_paths(root, &format!("{commit}^"), commit)
}

pub fn lock_integration(root: &Path) -> Result<File> {
    // The common Git directory also serializes callers using different state
    // directories or different worktrees of this repository.
    let common = root.join(checked(root, &["rev-parse", "--git-common-dir"])?);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(common.join("cadence-integration.lock"))?;
    lock.try_lock_exclusive()
        .context("another Cadence integration is in progress; retry when it finishes")?;
    Ok(lock)
}

fn git_path(root: &Path, name: &str) -> Result<PathBuf> {
    Ok(root.join(checked(root, &["rev-parse", "--git-path", name])?))
}

fn ensure_no_git_operation(root: &Path) -> Result<()> {
    for name in [
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "MERGE_HEAD",
        "sequencer",
        "rebase-merge",
        "rebase-apply",
    ] {
        anyhow::ensure!(
            !git_path(root, name)?.exists(),
            "Git operation already in progress ({name}); refusing to start or abort a cherry-pick"
        );
    }
    Ok(())
}

pub fn cherry_pick(root: &Path, commits: &[String]) -> Result<()> {
    if commits.is_empty() {
        bail!("Agent produced no commits");
    }
    ensure_no_git_operation(root)?;
    let original_head = head(root)?;
    let mut command = Command::new("git");
    command.arg("-C").arg(root).arg("cherry-pick").args(commits);
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let pick_head = git_path(root, "CHERRY_PICK_HEAD")?;
    let sequence_head = git_path(root, "sequencer/head")?;
    let owns_pick = pick_head.exists()
        && commits
            .iter()
            .any(|commit| fs::read_to_string(&pick_head).is_ok_and(|head| head.trim() == commit));
    let owns_sequence =
        sequence_head.exists() && fs::read_to_string(sequence_head)?.trim() == original_head;
    if owns_pick || owns_sequence {
        let abort = git(root, &["cherry-pick", "--abort"])?;
        if !abort.status.success() {
            bail!(
                "cherry-pick failed: {message}; abort also failed; Git state was retained: {}",
                String::from_utf8_lossy(&abort.stderr).trim()
            );
        }
        bail!("cherry-pick failed and was aborted: {message}");
    }
    bail!("cherry-pick failed: {message}; no owned operation was aborted")
}

pub fn delete_branch(root: &Path, branch: &str) -> Result<()> {
    anyhow::ensure!(
        branch.starts_with("cadence/"),
        "refusing to delete non-Cadence branch"
    );
    let reference = format!("refs/heads/{branch}");
    let exists = git(root, &["show-ref", "--verify", "--quiet", &reference])?;
    if exists.status.code() == Some(1) {
        return Ok(());
    }
    anyhow::ensure!(
        exists.status.success(),
        "failed to inspect Cadence branch {branch}"
    );
    checked(root, &["branch", "-D", branch])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn command(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        command(temp.path(), &["init", "-b", "main"]);
        command(
            temp.path(),
            &["config", "user.email", "cadence@example.test"],
        );
        command(temp.path(), &["config", "user.name", "Cadence Test"]);
        fs::write(temp.path().join("file.txt"), "base\n").unwrap();
        command(temp.path(), &["add", "file.txt"]);
        command(temp.path(), &["commit", "-m", "base"]);
        temp
    }

    #[test]
    fn cherry_picks_agent_commit() {
        let repo = repository();
        let agent_dir = tempfile::tempdir().unwrap();
        let agent_path = agent_dir.path().join("agent");
        command(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "cadence/test/agent-1",
                agent_path.to_str().unwrap(),
            ],
        );
        let base = head(repo.path()).unwrap();
        fs::write(agent_path.join("agent.txt"), "done\n").unwrap();
        command(&agent_path, &["add", "agent.txt"]);
        command(&agent_path, &["commit", "-m", "agent"]);
        let agent_head = head(&agent_path).unwrap();
        let commits = commits_between(&agent_path, &base, &agent_head).unwrap();
        assert_eq!(
            changed_paths(&agent_path, &base, &agent_head).unwrap(),
            ["agent.txt"]
        );
        cherry_pick(repo.path(), &commits).unwrap();
        assert_eq!(
            fs::read_to_string(repo.path().join("agent.txt")).unwrap(),
            "done\n"
        );
        ensure_clean(repo.path()).unwrap();
    }

    #[test]
    fn aborts_conflicting_cherry_pick() {
        let repo = repository();
        let agent_dir = tempfile::tempdir().unwrap();
        let agent_path = agent_dir.path().join("agent");
        command(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "cadence/test/agent-2",
                agent_path.to_str().unwrap(),
            ],
        );
        let base = head(repo.path()).unwrap();
        fs::write(agent_path.join("file.txt"), "agent\n").unwrap();
        command(&agent_path, &["add", "file.txt"]);
        command(&agent_path, &["commit", "-m", "agent conflict"]);
        let agent_head = head(&agent_path).unwrap();
        fs::write(repo.path().join("file.txt"), "lead\n").unwrap();
        command(repo.path(), &["add", "file.txt"]);
        command(repo.path(), &["commit", "-m", "base conflict"]);
        let before = head(repo.path()).unwrap();
        let commits = commits_between(&agent_path, &base, &agent_head).unwrap();
        assert!(cherry_pick(repo.path(), &commits).is_err());
        assert_eq!(head(repo.path()).unwrap(), before);
        ensure_clean(repo.path()).unwrap();
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "lead\n"
        );
    }

    #[test]
    fn resolves_commit_revisions_to_object_ids() {
        let repo = repository();

        assert_eq!(
            resolve_commit(repo.path(), "HEAD").unwrap(),
            head(repo.path()).unwrap()
        );
        assert!(resolve_commit(repo.path(), "--help").is_err());
    }

    #[test]
    fn integration_lock_is_shared_across_worktrees_and_released_on_drop() {
        let repo = repository();
        let other = tempfile::tempdir().unwrap();
        let checkout = other.path().join("checkout");
        command(
            repo.path(),
            &["worktree", "add", "-b", "other", checkout.to_str().unwrap()],
        );
        let lock = lock_integration(repo.path()).unwrap();
        assert!(lock_integration(&checkout).is_err());
        drop(lock);
        assert!(lock_integration(&checkout).is_ok());
    }

    #[test]
    fn preserves_an_existing_cherry_pick() {
        let repo = repository();
        command(repo.path(), &["checkout", "-b", "agent"]);
        fs::write(repo.path().join("file.txt"), "agent\n").unwrap();
        command(repo.path(), &["commit", "-am", "agent"]);
        let commit = head(repo.path()).unwrap();
        command(repo.path(), &["checkout", "main"]);
        fs::write(repo.path().join("file.txt"), "lead\n").unwrap();
        command(repo.path(), &["commit", "-am", "lead"]);
        assert!(
            !git(repo.path(), &["cherry-pick", &commit])
                .unwrap()
                .status
                .success()
        );
        let before_index = checked(repo.path(), &["ls-files", "--stage"]).unwrap();
        let before_file = fs::read(repo.path().join("file.txt")).unwrap();
        let error = cherry_pick(repo.path(), std::slice::from_ref(&commit)).unwrap_err();
        assert!(error.to_string().contains("already in progress"));
        assert_eq!(
            checked(repo.path(), &["rev-parse", "CHERRY_PICK_HEAD"]).unwrap(),
            commit
        );
        assert_eq!(
            checked(repo.path(), &["ls-files", "--stage"]).unwrap(),
            before_index
        );
        assert_eq!(fs::read(repo.path().join("file.txt")).unwrap(), before_file);
    }
}
