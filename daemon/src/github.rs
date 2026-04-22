use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub labels: Vec<Label>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    /// GraphQL node ID, used for adding reactions.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Label {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Comment {
    /// GraphQL node ID, used for adding reactions.
    #[serde(default)]
    pub id: Option<String>,
    pub author: CommentAuthor,
    pub body: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommentAuthor {
    pub login: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrComment {
    /// GraphQL node ID, used for adding reactions.
    #[serde(default)]
    pub id: Option<String>,
    pub author: CommentAuthor,
    pub body: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Review {
    pub author: CommentAuthor,
    pub body: String,
    pub state: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrStatus {
    pub number: u64,
    pub state: String,
    pub mergeable: String,
    #[serde(rename = "reviewDecision")]
    pub review_decision: String,
    #[serde(rename = "statusCheckRollup")]
    pub check_runs: Vec<CheckRun>,
    #[serde(default)]
    pub comments: Vec<PrComment>,
    #[serde(default)]
    pub reviews: Vec<Review>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CheckRun {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    #[serde(rename = "detailsUrl", default)]
    pub details_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FailedCheckLog {
    pub name: String,
    pub log: String,
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

async fn run_gh(args: &[&str]) -> Result<String> {
    debug!(cmd = "gh", ?args, "running gh command");

    let output = Command::new("gh")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn gh")?
        .wait_with_output()
        .await
        .context("failed to wait on gh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "gh {} failed (exit {}): {}",
            args.first().unwrap_or(&""),
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(output.stdout).context("gh stdout was not valid UTF-8")?;
    Ok(stdout)
}

async fn run_git(args: &[&str], dir: &Path) -> Result<String> {
    debug!(cmd = "git", ?args, ?dir, "running git command");

    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn git")?
        .wait_with_output()
        .await
        .context("failed to wait on git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {} failed (exit {}): {}",
            args.first().unwrap_or(&""),
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(output.stdout).context("git stdout was not valid UTF-8")?;
    Ok(stdout)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fetch all open issues in  that carry the given .
///
/// The returned issues will **not** have comments populated (the gh query
/// does not request them), but the field defaults to an empty vec thanks to
/// .
pub async fn fetch_labeled_issues(repo: &str, label: &str) -> Result<Vec<Issue>> {
    let json = run_gh(&[
        "issue",
        "list",
        "--repo",
        repo,
        "--label",
        label,
        "--state",
        "open",
        "--json",
        "number,title,body,state,labels",
        "--limit",
        "50",
    ])
    .await
    .context("fetch_labeled_issues")?;

    let issues: Vec<Issue> =
        serde_json::from_str(&json).context("failed to parse issue list JSON")?;
    Ok(issues)
}

/// Fetch full detail (including comments) for a single issue.
pub async fn fetch_issue_detail(repo: &str, number: u64) -> Result<Issue> {
    let number_str = number.to_string();
    let json = run_gh(&[
        "issue",
        "view",
        &number_str,
        "--repo",
        repo,
        "--json",
        "id,number,title,body,state,labels,comments",
    ])
    .await
    .context("fetch_issue_detail")?;

    let issue: Issue = serde_json::from_str(&json).context("failed to parse issue detail JSON")?;
    Ok(issue)
}

/// Ensure a bare clone of `repo` exists at `repos_dir/<owner>-<repo>.git`.
///
/// If the bare repo already exists, runs `git fetch --prune origin` to update
/// it. Otherwise, creates it with `gh repo clone ... -- --bare`.
/// Returns the path to the bare repo directory.
pub async fn ensure_bare_repo(repo: &str, repos_dir: &Path) -> Result<PathBuf> {
    let bare_name = format!("{}.git", repo.replace('/', "-"));
    let bare_path = repos_dir.join(&bare_name);
    let bare_str = bare_path
        .to_str()
        .context("ensure_bare_repo: bare repo path is not valid UTF-8")?
        .to_string();

    if bare_path.exists() {
        // Already cloned — fetch latest refs.
        tracing::info!(path = %bare_path.display(), "bare repo exists, fetching updates");
        if let Err(e) = run_git(&["fetch", "--prune", "origin"], &bare_path).await {
            tracing::warn!("bare repo fetch failed (will continue): {:#}", e);
        }
    } else {
        // Clone as bare.
        tracing::info!(repo, path = %bare_path.display(), "cloning bare repo");
        run_gh(&["repo", "clone", repo, &bare_str, "--", "--bare"])
            .await
            .context("ensure_bare_repo: failed to clone bare repo")?;
    }

    Ok(bare_path)
}

/// Create a git worktree at `worktree_dest` with branch `branch`.
///
/// If the branch already exists on origin, the worktree is based on
/// `origin/<branch>` (using `-B` to reset it). Otherwise, a new branch is
/// created from HEAD (the default branch).
pub async fn add_worktree(bare_repo: &Path, worktree_dest: &Path, branch: &str) -> Result<()> {
    let dest_str = worktree_dest
        .to_str()
        .context("add_worktree: worktree path is not valid UTF-8")?;

    // Check if the branch exists on origin.
    let remote_ref = format!("origin/{}", branch);
    let branch_on_remote = run_git(&["rev-parse", "--verify", &remote_ref], bare_repo)
        .await
        .is_ok();

    if branch_on_remote {
        // Branch exists on remote — base worktree on it (reset with -B).
        run_git(
            &["worktree", "add", dest_str, "-B", branch, &remote_ref],
            bare_repo,
        )
        .await
        .context("add_worktree: failed to add worktree from remote branch")?;
        tracing::info!(branch, "worktree created from existing remote branch");
    } else {
        // New branch — create from HEAD (default branch).
        run_git(&["worktree", "add", dest_str, "-b", branch], bare_repo)
            .await
            .context("add_worktree: failed to add worktree with new branch")?;
        tracing::info!(branch, "worktree created with new branch");
    }

    // Configure user identity in the worktree so commits work.
    let _ = run_git(
        &["config", "user.email", "familiar@users.noreply.github.com"],
        worktree_dest,
    )
    .await;
    let _ = run_git(&["config", "user.name", "Familiar"], worktree_dest).await;

    Ok(())
}

/// Remove a git worktree. Falls back to `fs::remove_dir_all` + `git worktree
/// prune` if the git command fails.
pub async fn remove_worktree(bare_repo: &Path, worktree_dest: &Path) -> Result<()> {
    let dest_str = worktree_dest
        .to_str()
        .context("remove_worktree: worktree path is not valid UTF-8")?;

    let result = run_git(&["worktree", "remove", "--force", dest_str], bare_repo).await;

    if result.is_err() {
        tracing::warn!(
            path = %worktree_dest.display(),
            "git worktree remove failed, falling back to rm + prune"
        );
        if worktree_dest.exists() {
            std::fs::remove_dir_all(worktree_dest).with_context(|| {
                format!("failed to remove worktree at {}", worktree_dest.display())
            })?;
        }
        let _ = run_git(&["worktree", "prune"], bare_repo).await;
    }

    tracing::info!(path = %worktree_dest.display(), "worktree removed");
    Ok(())
}

/// Resolve the actual `.git` directory for a worktree path.
///
/// In a worktree layout, `.git` is a file that points to the main repo's
/// `.git/worktrees/<name>/` directory. This helper returns the actual git dir
/// so lock files can be found correctly.
pub async fn resolve_git_dir(worktree: &Path) -> Result<PathBuf> {
    let output = run_git(&["rev-parse", "--git-dir"], worktree)
        .await
        .context("resolve_git_dir")?;
    let git_dir = output.trim();
    let path = std::path::Path::new(git_dir);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(worktree.join(path))
    }
}

/// Stage everything and commit with the given message.
/// Returns Ok(()) even if there is nothing to commit.
pub async fn commit_all(worktree: &Path, message: &str) -> Result<()> {
    run_git(&["add", "-A"], worktree)
        .await
        .context("commit_all: git add")?;

    // Check if there is anything to commit (git diff --cached --quiet exits 1 if there are changes).
    let has_changes = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(worktree)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("commit_all: failed to spawn git diff")?
        .wait()
        .await
        .context("commit_all: failed to wait on git diff")?;

    if has_changes.success() {
        // Exit 0 means no changes staged — nothing to commit.
        tracing::info!("nothing to commit, skipping");
        return Ok(());
    }

    run_git(&["commit", "-m", message], worktree)
        .await
        .context("commit_all: git commit")?;

    Ok(())
}

/// Push `branch` to origin, setting upstream tracking.
///
/// Tries `--force-with-lease` first (safe default). Falls back to `--force`
/// if tracking info is stale. Safe because familiar owns these branches exclusively.
pub async fn push_branch(worktree: &Path, branch: &str) -> Result<()> {
    // Refresh remote tracking info so --force-with-lease has accurate state.
    let _ = run_git(&["fetch", "origin", branch], worktree).await;
    let result = run_git(
        &["push", "--force-with-lease", "-u", "origin", branch],
        worktree,
    )
    .await;

    if result.is_ok() {
        return Ok(());
    }

    tracing::warn!(
        "--force-with-lease rejected push for {}, retrying with --force",
        branch
    );
    run_git(&["push", "--force", "-u", "origin", branch], worktree)
        .await
        .context("push_branch (force)")?;
    Ok(())
}

/// Create a pull request and return its number.
pub async fn create_pr(repo: &str, base: &str, head: &str, title: &str, body: &str) -> Result<u64> {
    // gh pr create prints the PR URL to stdout (e.g. https://github.com/owner/repo/pull/42)
    let url = run_gh(&[
        "pr", "create", "--repo", repo, "--base", base, "--head", head, "--title", title, "--body",
        body,
    ])
    .await
    .context("create_pr")?;

    // Parse the PR number from the URL: last path segment.
    let url = url.trim();
    let pr_number: u64 = url
        .rsplit('/')
        .next()
        .context("create_pr: no PR number in URL")?
        .parse()
        .with_context(|| format!("create_pr: failed to parse PR number from: {}", url))?;

    Ok(pr_number)
}

/// Find an existing pull request for the given head branch.
/// Returns `Some(pr_number)` if one exists, `None` otherwise.
pub async fn find_pr_for_branch(repo: &str, branch: &str) -> Result<Option<u64>> {
    let json = run_gh(&[
        "pr", "list", "--repo", repo, "--head", branch, "--json", "number", "--limit", "1",
    ])
    .await
    .context("find_pr_for_branch")?;

    let prs: Vec<serde_json::Value> =
        serde_json::from_str(&json).context("failed to parse PR list JSON")?;

    if let Some(pr) = prs.first() {
        let number = pr["number"]
            .as_u64()
            .context("find_pr_for_branch: PR number not found in response")?;
        Ok(Some(number))
    } else {
        Ok(None)
    }
}

/// Delete a remote branch (best-effort).
///
/// Used to clean up `familiar/issue-*` branches after a PR is merged.
/// Errors are logged but not propagated -- the branch may already have been
/// deleted by GitHub's auto-delete-on-merge setting.
pub async fn delete_remote_branch(repo: &str, branch: &str) {
    // We need a local clone to run `git push --delete`. Instead, use `gh api`
    // to delete the ref via the GitHub API, which doesn't need a local checkout.
    let git_ref = format!("heads/{}", branch);
    let result = run_gh(&[
        "api",
        "--method",
        "DELETE",
        &format!("repos/{}/git/refs/{}", repo, git_ref),
    ])
    .await;

    match result {
        Ok(_) => {
            tracing::info!(repo, branch, "deleted remote branch");
        }
        Err(e) => {
            tracing::debug!(
                repo,
                branch,
                "could not delete remote branch (may already be gone): {:#}",
                e
            );
        }
    }
}

/// Fetch the current status of a pull request (checks, review, mergeable).
pub async fn fetch_pr_status(repo: &str, pr_number: u64) -> Result<PrStatus> {
    let number_str = pr_number.to_string();
    let json = run_gh(&[
        "pr",
        "view",
        &number_str,
        "--repo",
        repo,
        "--json",
        "number,state,mergeable,reviewDecision,statusCheckRollup,comments,reviews",
    ])
    .await
    .context("fetch_pr_status")?;

    let status: PrStatus = serde_json::from_str(&json).context("failed to parse PR status JSON")?;
    Ok(status)
}

/// Maximum bytes of log output to keep per failed check.
const MAX_LOG_BYTES: usize = 16_384;
/// Maximum number of lines of log output to keep per failed check.
const MAX_LOG_LINES: usize = 200;

/// Extract a GitHub Actions run ID from a details URL.
///
/// Typical URL shapes:
/// - `https://github.com/{owner}/{repo}/actions/runs/{run_id}`
/// - `https://github.com/{owner}/{repo}/actions/runs/{run_id}/job/{job_id}`
fn extract_run_id(url: &str) -> Option<&str> {
    let marker = "/actions/runs/";
    let start = url.find(marker)? + marker.len();
    let rest = &url[start..];
    // Run ID ends at '/' or end-of-string.
    let end = rest.find('/').unwrap_or(rest.len());
    let id = &rest[..end];
    // Sanity: must be all digits.
    if id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() {
        Some(id)
    } else {
        None
    }
}

/// Truncate `text` to the last `MAX_LOG_LINES` lines and `MAX_LOG_BYTES` bytes.
fn truncate_log(text: &str) -> String {
    // First truncate to last N lines.
    let lines: Vec<&str> = text.lines().collect();
    let tail = if lines.len() > MAX_LOG_LINES {
        lines[lines.len() - MAX_LOG_LINES..].join("\n")
    } else {
        text.to_string()
    };

    // Then truncate to max bytes (from the end).
    if tail.len() > MAX_LOG_BYTES {
        let start = tail.len() - MAX_LOG_BYTES;
        // Find a safe UTF-8 boundary.
        let safe_start = tail.ceil_char_boundary(start);
        format!("…(truncated)\n{}", &tail[safe_start..])
    } else {
        tail
    }
}

/// Fetch CI failure logs for the given failed checks.
///
/// For each check whose `details_url` points to a GitHub Actions run, this
/// runs `gh run view <run_id> --log-failed` and returns the (truncated) output.
///
/// This is best-effort: if fetching logs for a particular check fails, that
/// check is silently skipped.
pub async fn fetch_failed_check_logs(
    repo: &str,
    failed_checks: &[&CheckRun],
) -> Vec<FailedCheckLog> {
    // Deduplicate run IDs — multiple check runs can belong to the same workflow run.
    let mut seen_run_ids = std::collections::HashSet::new();
    let mut results = Vec::new();

    for check in failed_checks {
        let run_id = match check.details_url.as_deref().and_then(extract_run_id) {
            Some(id) => id.to_string(),
            None => continue,
        };

        if !seen_run_ids.insert(run_id.clone()) {
            continue;
        }

        let log_result = run_gh(&["run", "view", &run_id, "--repo", repo, "--log-failed"]).await;

        match log_result {
            Ok(raw_log) => {
                let log = truncate_log(&raw_log);
                results.push(FailedCheckLog {
                    name: check.name.clone(),
                    log,
                });
            }
            Err(e) => {
                tracing::warn!(
                    check = %check.name,
                    run_id,
                    "failed to fetch CI logs: {:#}",
                    e
                );
            }
        }
    }

    results
}

/// Add the 👀 (eyes) reaction to a GitHub comment or issue by its node ID.
/// This is best-effort: failures are logged but not propagated.
pub async fn react_with_eyes(node_id: &str) {
    let query = format!(
        r#"mutation {{ addReaction(input: {{subjectId: "{}", content: EYES}}) {{ reaction {{ content }} }} }}"#,
        node_id
    );
    let result = run_gh(&["api", "graphql", "-f", &format!("query={}", query)]).await;
    match result {
        Ok(_) => tracing::info!(node_id, "added 👀 reaction"),
        Err(e) => tracing::debug!(node_id, "failed to add 👀 reaction (best-effort): {:#}", e),
    }
}

/// Build the GraphQL mutation query string for adding an eyes reaction.
/// Exposed for testing.
#[cfg(test)]
fn build_eyes_reaction_query(node_id: &str) -> String {
    format!(
        r#"mutation {{ addReaction(input: {{subjectId: "{}", content: EYES}}) {{ reaction {{ content }} }} }}"#,
        node_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_run_id_standard_url() {
        let url = "https://github.com/owner/repo/actions/runs/12345678";
        assert_eq!(extract_run_id(url), Some("12345678"));
    }

    #[test]
    fn test_extract_run_id_with_job() {
        let url = "https://github.com/owner/repo/actions/runs/12345678/job/99999";
        assert_eq!(extract_run_id(url), Some("12345678"));
    }

    #[test]
    fn test_extract_run_id_no_actions() {
        assert_eq!(extract_run_id("https://github.com/owner/repo/pull/1"), None);
    }

    #[test]
    fn test_extract_run_id_empty() {
        assert_eq!(extract_run_id(""), None);
    }

    #[test]
    fn test_truncate_log_short() {
        let log = "line1\nline2\nline3\n";
        let result = truncate_log(log);
        assert_eq!(result, log.to_string());
    }

    #[test]
    fn test_truncate_log_many_lines() {
        let lines: Vec<String> = (0..300).map(|i| format!("line {}", i)).collect();
        let log = lines.join("\n");
        let result = truncate_log(&log);
        let result_lines: Vec<&str> = result.lines().collect();
        assert_eq!(result_lines.len(), MAX_LOG_LINES);
        // Should contain the last lines.
        assert!(result.contains("line 299"));
        assert!(!result.contains("line 0\n"));
    }

    #[test]
    fn test_truncate_log_large_bytes() {
        // Create a string larger than MAX_LOG_BYTES but fewer than MAX_LOG_LINES lines.
        let line = "x".repeat(1000);
        let lines: Vec<String> = (0..20).map(|_| line.clone()).collect();
        let log = lines.join("\n");
        assert!(log.len() > MAX_LOG_BYTES);
        let result = truncate_log(&log);
        assert!(result.len() <= MAX_LOG_BYTES + 20); // +20 for the truncation prefix
    }

    #[test]
    fn test_build_eyes_reaction_query() {
        let query = build_eyes_reaction_query("IC_kwDOTest123");
        assert!(query.contains("IC_kwDOTest123"));
        assert!(query.contains("addReaction"));
        assert!(query.contains("EYES"));
        assert!(query.contains("subjectId"));
    }

    #[test]
    fn test_comment_deserializes_with_id() {
        let json = r#"{"id":"IC_kwDOTest","author":{"login":"user"},"body":"hello","createdAt":"2025-01-01T00:00:00Z"}"#;
        let comment: Comment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.id.as_deref(), Some("IC_kwDOTest"));
    }

    #[test]
    fn test_comment_deserializes_without_id() {
        let json =
            r#"{"author":{"login":"user"},"body":"hello","createdAt":"2025-01-01T00:00:00Z"}"#;
        let comment: Comment = serde_json::from_str(json).unwrap();
        assert!(comment.id.is_none());
    }

    #[test]
    fn test_issue_deserializes_with_node_id() {
        let json = r#"{"id":"I_kwDOTest","number":1,"title":"t","body":"b","state":"OPEN","labels":[],"comments":[]}"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.id.as_deref(), Some("I_kwDOTest"));
    }

    #[test]
    fn test_issue_deserializes_without_node_id() {
        let json =
            r#"{"number":1,"title":"t","body":"b","state":"OPEN","labels":[],"comments":[]}"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert!(issue.id.is_none());
    }

    #[test]
    fn test_bare_repo_name_computation() {
        // Verify the naming convention used by ensure_bare_repo.
        let repo = "owner/repo";
        let bare_name = format!("{}.git", repo.replace('/', "-"));
        assert_eq!(bare_name, "owner-repo.git");
    }

    #[test]
    fn test_bare_repo_name_with_org() {
        let repo = "my-org/my-repo";
        let bare_name = format!("{}.git", repo.replace('/', "-"));
        assert_eq!(bare_name, "my-org-my-repo.git");
    }
}
