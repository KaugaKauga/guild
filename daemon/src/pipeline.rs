use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::copilot;
use crate::github;
use crate::Config;

// ---------------------------------------------------------------------------
// Stage
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Stage {
    Ingest,
    Understand,
    Plan,
    Implement,
    Verify,
    Submit,
    Watch,
    Fix,
    Done,
    Failed(String),
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Pipeline {
    pub issue_number: u64,
    pub repo: String,
    pub stage: Stage,
    pub run_dir: PathBuf,
    pub worktree: PathBuf,
    pub pr_number: Option<u64>,
    pub blocker_fingerprint: Option<String>,
    pub branch_name: String,
}

impl Pipeline {
    /// Create a new pipeline for the given issue.
    ///
    /// A run directory is created under `runs_dir` with the pattern
    /// `{timestamp}-{repo_slug}-{issue_number}`.
    pub fn new(issue_number: u64, repo: String, runs_dir: &Path) -> Self {
        let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let repo_slug = repo.replace('/', "-");
        let dir_name = format!("{}-{}-{}", timestamp, repo_slug, issue_number);
        let run_dir = runs_dir.join(&dir_name);
        let worktree = run_dir.join("worktree");
        let branch_name = format!("guild/issue-{}", issue_number);

        fs::create_dir_all(&run_dir).expect("failed to create run_dir");

        Self {
            issue_number,
            repo,
            stage: Stage::Ingest,
            run_dir,
            worktree,
            pr_number: None,
            blocker_fingerprint: None,
            branch_name,
        }
    }

    // ---------------------------------------------------------------------
    // Main state-machine driver
    // ---------------------------------------------------------------------

    /// Advance the pipeline by one stage.
    ///
    /// Returns `Ok(true)` when progress was made (stage changed) and
    /// `Ok(false)` when no progress occurred (e.g. waiting in Watch).
    pub async fn advance(&mut self, config: &Config) -> Result<bool> {
        match &self.stage {
            Stage::Ingest => self.do_ingest().await,
            Stage::Understand => self.do_understand().await,
            Stage::Plan => self.do_plan(config).await,
            Stage::Implement => self.do_implement(config).await,
            Stage::Verify => self.do_verify(config).await,
            Stage::Submit => self.do_submit().await,
            Stage::Watch => self.do_watch().await,
            Stage::Fix => self.do_fix(config).await,
            Stage::Done => Ok(false),
            Stage::Failed(_) => Ok(false),
        }
    }

    // ------------------------------------------------------------------
    // Convenience predicates
    // ------------------------------------------------------------------

    pub fn is_done(&self) -> bool {
        matches!(self.stage, Stage::Done)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.stage, Stage::Failed(_))
    }

    // ------------------------------------------------------------------
    // Stage implementations
    // ------------------------------------------------------------------

    async fn do_ingest(&mut self) -> Result<bool> {
        let issue = github::fetch_issue_detail(&self.repo, self.issue_number)
            .await
            .context("Ingest: failed to fetch issue detail")?;

        // Persist the full issue JSON.
        let issue_json = serde_json::to_string_pretty(&issue)
            .context("Ingest: failed to serialise issue")?;
        fs::write(self.run_dir.join("issue.json"), &issue_json)
            .context("Ingest: failed to write issue.json")?;

        // Persist issue body.
        let body = issue.body.as_deref().unwrap_or("");
        fs::write(self.run_dir.join("issue_body.md"), body)
            .context("Ingest: failed to write issue_body.md")?;

        // Persist comments.
        let comments_json = serde_json::to_string_pretty(&issue.comments)
            .context("Ingest: failed to serialise comments")?;
        fs::write(self.run_dir.join("issue_comments.json"), &comments_json)
            .context("Ingest: failed to write issue_comments.json")?;

        info!(
            "Ingested issue #{}: {}",
            issue.number, issue.title
        );

        self.stage = Stage::Understand;
        Ok(true)
    }

    async fn do_understand(&mut self) -> Result<bool> {
        // Clone repo into worktree if it doesn't already exist.
        if !self.worktree.exists() {
            github::clone_repo(&self.repo, &self.worktree)
                .await
                .context("Understand: failed to clone repo")?;
        }

        // Scan for notable files.
        let ci_workflows = scan_glob(&self.worktree, ".github/workflows", "yml");
        let contributing_docs = scan_known_files(
            &self.worktree,
            &["CONTRIBUTING.md", "AGENTS.md"],
        );
        let build_files = scan_known_files(
            &self.worktree,
            &["package.json", "Cargo.toml", "go.mod", "pyproject.toml"],
        );

        let tree = dir_tree(&self.worktree, 2)
            .context("Understand: failed to build dir tree")?;

        // Write repo_summary.md
        let mut summary = String::new();
        summary.push_str("# Repo Summary\n\n");

        summary.push_str("## CI Workflow Files\n");
        if ci_workflows.is_empty() {
            summary.push_str("- (none found)\n");
        } else {
            for f in &ci_workflows {
                summary.push_str(&format!("- {}\n", f));
            }
        }

        summary.push_str("\n## Contributing / Agent Docs\n");
        if contributing_docs.is_empty() {
            summary.push_str("- (none found)\n");
        } else {
            for f in &contributing_docs {
                summary.push_str(&format!("- {}\n", f));
            }
        }

        summary.push_str("\n## Build / Project Files\n");
        if build_files.is_empty() {
            summary.push_str("- (none found)\n");
        } else {
            for f in &build_files {
                summary.push_str(&format!("- {}\n", f));
            }
        }

        // Check for repo-specific learnings.
        let learnings_path = self.worktree.join(".guild").join("learnings.md");
        let learnings = if learnings_path.exists() {
            fs::read_to_string(&learnings_path).unwrap_or_default()
        } else {
            String::new()
        };

        // Persist learnings to run dir for later stages.
        if !learnings.is_empty() {
            fs::write(self.run_dir.join("learnings.md"), &learnings)
                .context("Understand: failed to write learnings.md")?;
        }

        summary.push_str("\n## Repo Learnings\n");
        if learnings.is_empty() {
            summary.push_str("- (no .guild/learnings.md found)\n");
        } else {
            summary.push_str(&learnings);
            summary.push('\n');
        }

        summary.push_str("\n## Directory Tree (depth 2)\n```\n");
        summary.push_str(&tree);
        summary.push_str("```\n");

        fs::write(self.run_dir.join("repo_summary.md"), &summary)
            .context("Understand: failed to write repo_summary.md")?;

        // Create branch.
        github::create_branch(&self.worktree, &self.branch_name)
            .await
            .context("Understand: failed to create branch")?;

        info!("Repo understood, branch created: {}", self.branch_name);

        self.stage = Stage::Plan;
        Ok(true)
    }

    async fn do_plan(&mut self, config: &Config) -> Result<bool> {
        let issue_body = read_file_or(
            &self.run_dir.join("issue_body.md"),
            "(no issue body)",
        );
        let repo_summary = read_file_or(
            &self.run_dir.join("repo_summary.md"),
            "(no repo summary)",
        );

        let learnings = read_file_or(
            &self.run_dir.join("learnings.md"),
            "",
        );

        let prompt = format!(
            "You are an autonomous coding agent in the PLAN stage.\n\
             \n\
             ## Your Task\n\
             Read the GitHub issue and repo context below, then create a detailed implementation plan.\n\
             \n\
             ## Issue\n\
             {issue_body}\n\
             \n\
             ## Repo Structure\n\
             {repo_summary}\n\
             \n\
             ## Repo Learnings (IMPORTANT — read before planning)\n\
             {learnings}\n\
             \n\
             ## Instructions\n\
             1. Read the issue carefully -- understand acceptance criteria\n\
             2. Trace the user path -- what component does this touch?\n\
             3. Write your plan to {plan_path}\n\
             4. Include: files to create/modify, tests to write, UI wiring needed\n",
            issue_body = issue_body,
            repo_summary = repo_summary,
            learnings = learnings,
            plan_path = self.run_dir.join("plan.md").display(),
        );

        let prompt_path = self.run_dir.join("prompt_plan.md");
        fs::write(&prompt_path, &prompt)
            .context("Plan: failed to write prompt_plan.md")?;

        copilot::run_copilot(&config.copilot_cmd, &prompt_path, &self.worktree)
            .await
            .context("Plan: copilot run failed")?;

        self.stage = Stage::Implement;
        Ok(true)
    }

    async fn do_implement(&mut self, config: &Config) -> Result<bool> {
        let issue_body = read_file_or(
            &self.run_dir.join("issue_body.md"),
            "(no issue body)",
        );
        let plan = read_file_or(
            &self.run_dir.join("plan.md"),
            "No plan file found -- read the issue and implement directly.",
        );
        let learnings = read_file_or(
            &self.run_dir.join("learnings.md"),
            "",
        );

        let prompt = format!(
            "You are an autonomous coding agent in the IMPLEMENT stage.\n\
             \n\
             ## Your Task\n\
             Implement the changes described in the plan.\n\
             \n\
             ## Issue\n\
             {issue_body}\n\
             \n\
             ## Plan\n\
             {plan}\n\
             \n\
             ## Repo Learnings (IMPORTANT — read before implementing)\n\
             {learnings}\n\
             \n\
             ## Instructions\n\
             1. Write the code in the worktree at: {worktree}\n\
             2. Write tests\n\
             3. Wire it into the UI so a user can actually see/use it\n\
             4. Do NOT commit -- the system will handle that\n",
            issue_body = issue_body,
            plan = plan,
            learnings = learnings,
            worktree = self.worktree.display(),
        );

        let prompt_path = self.run_dir.join("prompt_implement.md");
        fs::write(&prompt_path, &prompt)
            .context("Implement: failed to write prompt_implement.md")?;

        copilot::run_copilot(&config.copilot_cmd, &prompt_path, &self.worktree)
            .await
            .context("Implement: copilot run failed")?;

        self.stage = Stage::Verify;
        Ok(true)
    }

    async fn do_verify(&mut self, config: &Config) -> Result<bool> {
        let prompt = format!(
            "You are an autonomous coding agent in the VERIFY stage.\n\
             \n\
             ## Your Task\n\
             Run linting and basic checks on the code you just wrote.\n\
             \n\
             ## Instructions\n\
             1. Look at the repo's package.json / Cargo.toml / Makefile for lint commands\n\
             2. Run linting (e.g., npm run lint, cargo clippy, etc.)\n\
             3. Do NOT run the full test suite if it might hang (watch mode, browser tests)\n\
             4. Fix any lint errors you find\n\
             5. Write a summary of what you checked to {verify_report}\n",
            verify_report = self.run_dir.join("verify_report.md").display(),
        );

        let prompt_path = self.run_dir.join("prompt_verify.md");
        fs::write(&prompt_path, &prompt)
            .context("Verify: failed to write prompt_verify.md")?;

        copilot::run_copilot(&config.copilot_cmd, &prompt_path, &self.worktree)
            .await
            .context("Verify: copilot run failed")?;

        self.stage = Stage::Submit;
        Ok(true)
    }

    async fn do_submit(&mut self) -> Result<bool> {
        let commit_msg = format!("guild: implement issue #{}", self.issue_number);

        github::commit_all(&self.worktree, &commit_msg)
            .await
            .context("Submit: failed to commit")?;

        github::push_branch(&self.worktree, &self.branch_name)
            .await
            .context("Submit: failed to push branch")?;

        let pr_title = format!("guild: issue #{}", self.issue_number);
        let pr_body = format!(
            "Automated implementation for #{}\n\nGenerated by guild.",
            self.issue_number,
        );

        let pr_number = github::create_draft_pr(
            &self.repo,
            "main",
            &self.branch_name,
            &pr_title,
            &pr_body,
        )
        .await
        .context("Submit: failed to create draft PR")?;

        info!("Draft PR #{} created", pr_number);
        self.pr_number = Some(pr_number);
        self.stage = Stage::Watch;
        Ok(true)
    }

    async fn do_watch(&mut self) -> Result<bool> {
        let pr_number = self
            .pr_number
            .ok_or_else(|| anyhow::anyhow!("Watch: no PR number set"))?;

        let status = github::fetch_pr_status(&self.repo, pr_number)
            .await
            .context("Watch: failed to fetch PR status")?;

        // Collect failed checks.
        let failed_checks: Vec<&github::CheckRun> = status
            .check_runs
            .iter()
            .filter(|c| c.status == "completed" && c.conclusion != Some("success".to_string()))
            .collect();

        // Collect @guild-mentioned comments (regular PR comments that summon the agent).
        let guild_comments: Vec<&github::PrComment> = status
            .comments
            .iter()
            .filter(|c| {
                let login = &c.author.login;
                let is_human = !login.ends_with("[bot]") && login != "github-actions";
                is_human && c.body.contains("@guild")
            })
            .collect();

        // Collect formal review bodies when changes are requested.
        let review_comments: Vec<&github::Review> = if status.review_decision == "CHANGES_REQUESTED" {
            status.reviews.iter()
                .filter(|r| r.state == "CHANGES_REQUESTED")
                .collect()
        } else {
            Vec::new()
        };

        // Build fingerprint from all actionable signals.
        let mut fingerprint_input = String::new();
        for c in &failed_checks {
            fingerprint_input.push_str(&c.name);
            fingerprint_input.push(';');
        }
        fingerprint_input.push_str(&status.review_decision);
        fingerprint_input.push(';');
        fingerprint_input.push_str(&status.mergeable);
        fingerprint_input.push(';');
        for c in &guild_comments {
            fingerprint_input.push_str(&c.author.login);
            fingerprint_input.push(':');
            fingerprint_input.push_str(&c.body);
            fingerprint_input.push(';');
        }
        for r in &review_comments {
            fingerprint_input.push_str(&r.author.login);
            fingerprint_input.push(':');
            fingerprint_input.push_str(&r.body);
            fingerprint_input.push(';');
        }

        let mut hasher = Sha256::new();
        hasher.update(fingerprint_input.as_bytes());
        let fingerprint = hex::encode(hasher.finalize());

        // Done condition: all checks green, review approved (or none), state open,
        // and no @guild comments or change-request reviews pending.
        let all_checks_pass = failed_checks.is_empty();
        let review_ok =
            status.review_decision == "APPROVED" || status.review_decision.is_empty();
        let state_ok = status.state == "OPEN" || status.state == "open";
        let no_actionable = guild_comments.is_empty() && review_comments.is_empty();

        if all_checks_pass && review_ok && state_ok && no_actionable {
            info!("PR #{} is green with no actionable feedback! Marking done.", pr_number);
            self.stage = Stage::Done;
            return Ok(true);
        }

        // Check if blocker fingerprint changed.
        let changed = match &self.blocker_fingerprint {
            Some(prev) => prev != &fingerprint,
            None => true,
        };

        if changed {
            self.blocker_fingerprint = Some(fingerprint);

            // Write blocker report.
            let mut report = String::from("# Blocker Report\n\n");

            report.push_str("## Failed Checks\n");
            if failed_checks.is_empty() {
                report.push_str("- (none)\n");
            } else {
                for c in &failed_checks {
                    let conclusion = c.conclusion.as_deref().unwrap_or("unknown");
                    report.push_str(&format!("- **{}**: {}\n", c.name, conclusion));
                }
            }

            report.push_str(&format!(
                "\n## Review Decision\n{}\n",
                if status.review_decision.is_empty() {
                    "(none)"
                } else {
                    &status.review_decision
                },
            ));
            report.push_str(&format!("\n## Mergeable State\n{}\n", status.mergeable));

            if !review_comments.is_empty() {
                report.push_str("\n## Review Comments (changes requested)\n");
                for r in &review_comments {
                    report.push_str(&format!(
                        "\n### @{} ({}) — {}\n{}\n",
                        r.author.login, r.created_at, r.state, r.body
                    ));
                }
            }

            if !guild_comments.is_empty() {
                report.push_str("\n## @guild Mentions\n");
                for c in &guild_comments {
                    report.push_str(&format!(
                        "\n### @{} ({})\n{}\n",
                        c.author.login, c.created_at, c.body
                    ));
                }
            }

            fs::write(self.run_dir.join("blocker_report.md"), &report)
                .context("Watch: failed to write blocker_report.md")?;

            info!("Blockers changed on PR #{}, entering FIX", pr_number);
            self.stage = Stage::Fix;
            return Ok(true);
        }

        // No change -- will check again next poll.
        Ok(false)
    }

    async fn do_fix(&mut self, config: &Config) -> Result<bool> {
        let blocker_report = read_file_or(
            &self.run_dir.join("blocker_report.md"),
            "(no blocker report)",
        );
        let issue_body = read_file_or(
            &self.run_dir.join("issue_body.md"),
            "(no issue body)",
        );
        let learnings = read_file_or(
            &self.run_dir.join("learnings.md"),
            "",
        );

        let prompt = format!(
            "You are an autonomous coding agent in the FIX stage.\n\
             \n\
             ## Your Task\n\
             Fix the issues blocking the PR.\n\
             \n\
             ## Blocker Report\n\
             {blocker_report}\n\
             \n\
             ## Original Issue\n\
             {issue_body}\n\
             \n\
             ## Repo Learnings (IMPORTANT — read before fixing)\n\
             {learnings}\n\
             \n\
             ## Instructions\n\
             1. Read the blocker report above\n\
             2. Fix failing checks, address review comments\n\
             3. Do NOT commit -- the system will handle that\n",
            blocker_report = blocker_report,
            issue_body = issue_body,
            learnings = learnings,
        );

        let prompt_path = self.run_dir.join("prompt_fix.md");
        fs::write(&prompt_path, &prompt)
            .context("Fix: failed to write prompt_fix.md")?;

        copilot::run_copilot(&config.copilot_cmd, &prompt_path, &self.worktree)
            .await
            .context("Fix: copilot run failed")?;

        github::commit_all(&self.worktree, "guild: fix blockers")
            .await
            .context("Fix: failed to commit")?;

        github::push_branch(&self.worktree, &self.branch_name)
            .await
            .context("Fix: failed to push branch")?;

        self.stage = Stage::Watch;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recursively list directory contents up to `max_depth`, returning a
/// formatted string with indentation. Skips common noise directories.
pub fn dir_tree(path: &Path, max_depth: usize) -> Result<String> {
    let mut buf = String::new();
    dir_tree_inner(path, "", max_depth, 0, &mut buf)?;
    Ok(buf)
}

fn dir_tree_inner(
    path: &Path,
    prefix: &str,
    max_depth: usize,
    current_depth: usize,
    buf: &mut String,
) -> Result<()> {
    const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "dist", "build"];

    if current_depth > max_depth {
        return Ok(());
    }

    let mut entries: Vec<_> = fs::read_dir(path)
        .with_context(|| format!("dir_tree: failed to read {}", path.display()))?
        .filter_map(|e| e.ok())
        .collect();

    entries.sort_by_key(|e| e.file_name());

    for entry in &entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if SKIP_DIRS.contains(&name_str.as_ref()) {
            continue;
        }

        let file_type = entry
            .file_type()
            .context("dir_tree: failed to get file type")?;

        if file_type.is_dir() {
            buf.push_str(&format!("{}{}/\n", prefix, name_str));
            if current_depth < max_depth {
                dir_tree_inner(
                    &entry.path(),
                    &format!("{}  ", prefix),
                    max_depth,
                    current_depth + 1,
                    buf,
                )?;
            }
        } else {
            buf.push_str(&format!("{}{}\n", prefix, name_str));
        }
    }

    Ok(())
}


/// Read a file to string, returning `fallback` if the file doesn't exist.
fn read_file_or(path: &Path, fallback: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|_| fallback.to_string())
}

/// Scan a directory for files matching a given extension.
/// Returns a list of relative paths (relative to `base`).
fn scan_glob(base: &Path, subdir: &str, extension: &str) -> Vec<String> {
    let dir = base.join(subdir);
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == extension || path.to_string_lossy().ends_with(&format!(".{}", extension)) {
                    let rel = format!("{}/{}", subdir, entry.file_name().to_string_lossy());
                    results.push(rel);
                }
            }
        }
    }
    results.sort();
    results
}

/// Check which of the given known files exist under `base`.
/// Returns a list of the ones that exist.
fn scan_known_files(base: &Path, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|name| base.join(name).exists())
        .map(|name| name.to_string())
        .collect()
}
