use std::path::Path;
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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Label {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Comment {
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
        "number,title,body,state,labels,comments",
    ])
    .await
    .context("fetch_issue_detail")?;

    let issue: Issue = serde_json::from_str(&json).context("failed to parse issue detail JSON")?;
    Ok(issue)
}

/// Shallow-clone  into .
pub async fn clone_repo(repo: &str, dest: &Path) -> Result<()> {
    let dest_str = dest
        .to_str()
        .context("clone_repo: destination path is not valid UTF-8")?;

    run_gh(&[
        "repo",
        "clone",
        repo,
        dest_str,
        "--",
        "--depth=1",
        "--single-branch",
    ])
    .await
    .context("clone_repo")?;

    Ok(())
}

/// Check out an existing remote branch, or create a new local branch if it
/// does not exist on the remote.
///
/// This avoids the situation where a fresh shallow clone tries to create a
/// branch that already exists on origin, which would later cause a push
/// rejection.
pub async fn checkout_or_create_branch(worktree: &Path, branch: &str) -> Result<()> {
    // Try to fetch the branch from origin.
    let fetch_result = run_git(&["fetch", "origin", branch], worktree).await;

    if fetch_result.is_ok() {
        // Branch exists on remote — check it out tracking the remote.
        run_git(
            &["checkout", "-b", branch, &format!("origin/{}", branch)],
            worktree,
        )
        .await
        .context("checkout_or_create_branch: failed to checkout existing remote branch")?;
        tracing::info!("checked out existing remote branch: {}", branch);
    } else {
        // Branch does not exist on remote — create a new one.
        run_git(&["checkout", "-b", branch], worktree)
            .await
            .context("checkout_or_create_branch: failed to create new branch")?;
        tracing::info!("created new branch: {}", branch);
    }

    Ok(())
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

/// Push  to origin, setting upstream tracking.
pub async fn push_branch(worktree: &Path, branch: &str) -> Result<()> {
    run_git(&["push", "--force-with-lease", "-u", "origin", branch], worktree)
        .await
        .context("push_branch")?;
    Ok(())
}

/// Create a draft pull request and return its number.
pub async fn create_draft_pr(
    repo: &str,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
) -> Result<u64> {
    // gh pr create prints the PR URL to stdout (e.g. https://github.com/owner/repo/pull/42)
    let url = run_gh(&[
        "pr", "create", "--repo", repo, "--base", base, "--head", head, "--title", title, "--body",
        body, "--draft",
    ])
    .await
    .context("create_draft_pr")?;

    // Parse the PR number from the URL: last path segment.
    let url = url.trim();
    let pr_number: u64 = url
        .rsplit('/')
        .next()
        .context("create_draft_pr: no PR number in URL")?
        .parse()
        .with_context(|| format!("create_draft_pr: failed to parse PR number from: {}", url))?;

    Ok(pr_number)
}

/// Find an existing pull request for the given head branch.
/// Returns `Some(pr_number)` if one exists, `None` otherwise.
pub async fn find_pr_for_branch(repo: &str, branch: &str) -> Result<Option<u64>> {
    let json = run_gh(&[
        "pr", "list",
        "--repo", repo,
        "--head", branch,
        "--json", "number",
        "--limit", "1",
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
