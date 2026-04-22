use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::agent;
use crate::github;
use crate::Config;

// ---------------------------------------------------------------------------
// Agent prompt loader
// ---------------------------------------------------------------------------

/// Load an agent prompt template from disk and perform variable substitution.
///
/// Reads `{agents_dir}/{stage_name}.md`, then replaces each `{key}` in `vars`
/// with the corresponding value.
pub fn load_agent_prompt(
    agents_dir: &Path,
    stage_name: &str,
    vars: &[(&str, &str)],
) -> Result<String> {
    let template_path = agents_dir.join(format!("{}.md", stage_name));
    let mut prompt = fs::read_to_string(&template_path)
        .with_context(|| format!("failed to load agent template: {}", template_path.display()))?;

    for (key, value) in vars {
        let placeholder = format!("{{{}}}", key);
        prompt = prompt.replace(&placeholder, value);
    }

    Ok(prompt)
}

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

impl Stage {
    /// Position of this stage in the pipeline (0-based ordinal for progress).
    /// Failed maps to 0 since it can happen at any point.
    pub fn ordinal(&self) -> u8 {
        match self {
            Stage::Ingest => 1,
            Stage::Understand => 2,
            Stage::Plan => 3,
            Stage::Implement => 4,
            Stage::Verify => 5,
            Stage::Submit => 6,
            Stage::Watch => 7,
            Stage::Fix => 5, // fix loops back to roughly verify-level
            Stage::Done => 8,
            Stage::Failed(_) => 0,
        }
    }

    /// Total number of stages for progress bar calculation.
    pub fn total_stages() -> u8 {
        8
    }

    /// Returns true if this stage requires the agent CLI to run.
    /// These stages acquire a semaphore permit in the orchestrator.
    pub fn needs_agent(&self) -> bool {
        matches!(
            self,
            Stage::Plan | Stage::Implement | Stage::Verify | Stage::Fix
        )
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stage::Ingest => write!(f, "Ingest"),
            Stage::Understand => write!(f, "Understand"),
            Stage::Plan => write!(f, "Plan"),
            Stage::Implement => write!(f, "Implement"),
            Stage::Verify => write!(f, "Verify"),
            Stage::Submit => write!(f, "Submit"),
            Stage::Watch => write!(f, "Watch"),
            Stage::Fix => write!(f, "Fix"),
            Stage::Done => write!(f, "Done"),
            Stage::Failed(msg) => write!(f, "Failed: {}", msg),
        }
    }
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
    pub bare_repo: PathBuf,
    pub pr_number: Option<u64>,
    pub blocker_fingerprint: Option<String>,
    pub branch_name: String,
    pub issue_title: String,
}

impl Pipeline {
    /// Create a new pipeline for the given issue.
    ///
    /// A run directory is created under `runs_dir` with the pattern
    /// `{timestamp}-{repo_slug}-{issue_number}`.  The `bare_repo` path
    /// is computed under `repos_dir` (shared bare clone per repo).
    pub fn new(issue_number: u64, repo: String, runs_dir: &Path, repos_dir: &Path) -> Self {
        let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let repo_slug = repo.replace('/', "-");
        let dir_name = format!("{}-{}-{}", timestamp, repo_slug, issue_number);
        let run_dir = runs_dir.join(&dir_name);
        let worktree = run_dir.join("worktree");
        let branch_name = format!("guild/issue-{}", issue_number);
        let bare_name = format!("{}.git", repo_slug);
        let bare_repo = repos_dir.join(&bare_name);

        fs::create_dir_all(&run_dir).expect("failed to create run_dir");

        Self {
            issue_number,
            repo,
            stage: Stage::Ingest,
            run_dir,
            worktree,
            bare_repo,
            pr_number: None,
            blocker_fingerprint: None,
            branch_name,
            issue_title: String::new(),
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

    /// Remove the worktree and the entire run directory.
    ///
    /// Called after the pipeline has been recorded in the completed ledger.
    /// Errors are logged but not propagated -- cleanup is best-effort.
    #[allow(dead_code)]
    pub async fn cleanup_run(&self) {
        // Remove worktree via git worktree remove (falls back to rm internally).
        if self.worktree.exists() {
            match github::remove_worktree(&self.bare_repo, &self.worktree).await {
                Ok(_) => {
                    info!(
                        issue = self.issue_number,
                        path = %self.worktree.display(),
                        "removed worktree"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        issue = self.issue_number,
                        path = %self.worktree.display(),
                        "failed to remove worktree: {:#}", e
                    );
                }
            }
        }

        // Remove the entire run_dir (issue.json, prompts, reports, etc.).
        if self.run_dir.exists() {
            if let Err(e) = fs::remove_dir_all(&self.run_dir) {
                tracing::warn!(
                    issue = self.issue_number,
                    path = %self.run_dir.display(),
                    "failed to remove run_dir: {:#}", e
                );
            } else {
                info!(
                    issue = self.issue_number,
                    path = %self.run_dir.display(),
                    "removed run_dir"
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // Stage implementations
    // ------------------------------------------------------------------

    pub async fn do_ingest(&mut self) -> Result<bool> {
        let issue = github::fetch_issue_detail(&self.repo, self.issue_number)
            .await
            .context("Ingest: failed to fetch issue detail")?;

        // React with 👀 to acknowledge the issue is being worked on.
        if let Some(ref node_id) = issue.id {
            github::react_with_eyes(node_id).await;
        }

        // Persist the full issue JSON.
        let issue_json =
            serde_json::to_string_pretty(&issue).context("Ingest: failed to serialise issue")?;
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

        self.issue_title = issue.title.clone();

        info!("Ingested issue #{}: {}", issue.number, issue.title);

        self.stage = Stage::Understand;
        Ok(true)
    }

    pub async fn do_understand(&mut self) -> Result<bool> {
        // Create worktree from bare repo if it doesn't already exist.
        if !self.worktree.exists() {
            // Ensure bare clone exists and is up-to-date.
            let repos_dir = self
                .bare_repo
                .parent()
                .context("Understand: bare_repo has no parent directory")?;
            let _bare = github::ensure_bare_repo(&self.repo, repos_dir)
                .await
                .context("Understand: failed to ensure bare repo")?;

            // Create worktree with the working branch.
            github::add_worktree(&self.bare_repo, &self.worktree, &self.branch_name)
                .await
                .context("Understand: failed to add worktree")?;
        } else {
            // Worktree already exists (e.g. daemon restart). Clean up stale
            // lock files — in a worktree layout the git dir is different from
            // `.git/` so we resolve it dynamically.
            if let Ok(git_dir) = github::resolve_git_dir(&self.worktree).await {
                let lock: PathBuf = git_dir.join("index.lock");
                if lock.exists() {
                    info!("removing stale git lock file: {}", lock.display());
                    let _ = fs::remove_file(&lock);
                }
            }
        }

        // Scan for notable files.
        let ci_workflows = scan_glob(&self.worktree, ".github/workflows", "yml");
        let contributing_docs = scan_known_files(&self.worktree, &["CONTRIBUTING.md", "AGENTS.md"]);
        let build_files = scan_known_files(
            &self.worktree,
            &["package.json", "Cargo.toml", "go.mod", "pyproject.toml"],
        );

        let tree = dir_tree(&self.worktree, 2).context("Understand: failed to build dir tree")?;

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

        info!("Repo understood, branch created: {}", self.branch_name);

        self.stage = Stage::Plan;
        Ok(true)
    }

    pub async fn do_plan(&mut self, config: &Config) -> Result<bool> {
        let issue_body = read_file_or(&self.run_dir.join("issue_body.md"), "(no issue body)");
        let repo_summary = read_file_or(&self.run_dir.join("repo_summary.md"), "(no repo summary)");
        let learnings = read_file_or(&self.run_dir.join("learnings.md"), "");
        let plan_path = self.run_dir.join("plan.md").display().to_string();

        let prompt = load_agent_prompt(
            &config.agents_dir,
            "plan",
            &[
                ("issue_body", &issue_body),
                ("repo_summary", &repo_summary),
                ("learnings", &learnings),
                ("plan_path", &plan_path),
            ],
        )
        .context("Plan: failed to load agent template")?;

        let prompt_path = self.run_dir.join("prompt_plan.md");
        fs::write(&prompt_path, &prompt).context("Plan: failed to write prompt_plan.md")?;

        agent::run(
            config.backend,
            &config.agent_cmd,
            &config.model,
            &prompt_path,
            &self.worktree,
            &self.run_dir,
        )
        .await
        .context("Plan: agent run failed")?;

        self.stage = Stage::Implement;
        Ok(true)
    }

    pub async fn do_implement(&mut self, config: &Config) -> Result<bool> {
        let issue_body = read_file_or(&self.run_dir.join("issue_body.md"), "(no issue body)");
        let plan = read_file_or(
            &self.run_dir.join("plan.md"),
            "No plan file found -- read the issue and implement directly.",
        );

        // Prefer learnings from the worktree (may have been updated by a
        // previous agent), falling back to the snapshot taken during UNDERSTAND.
        let worktree_learnings = self.worktree.join(".guild").join("learnings.md");
        let learnings = if worktree_learnings.exists() {
            read_file_or(&worktree_learnings, "")
        } else {
            read_file_or(&self.run_dir.join("learnings.md"), "")
        };
        let worktree = self.worktree.display().to_string();
        let desc_path = self.run_dir.join("pr_description.md").display().to_string();

        let prompt = load_agent_prompt(
            &config.agents_dir,
            "implement",
            &[
                ("issue_body", &issue_body),
                ("plan", &plan),
                ("learnings", &learnings),
                ("worktree", &worktree),
                ("desc_path", &desc_path),
            ],
        )
        .context("Implement: failed to load agent template")?;

        let prompt_path = self.run_dir.join("prompt_implement.md");
        fs::write(&prompt_path, &prompt)
            .context("Implement: failed to write prompt_implement.md")?;

        agent::run(
            config.backend,
            &config.agent_cmd,
            &config.model,
            &prompt_path,
            &self.worktree,
            &self.run_dir,
        )
        .await
        .context("Implement: agent run failed")?;

        self.stage = Stage::Verify;
        Ok(true)
    }

    pub async fn do_verify(&mut self, config: &Config) -> Result<bool> {
        let verify_report = self.run_dir.join("verify_report.md").display().to_string();

        let prompt = load_agent_prompt(
            &config.agents_dir,
            "verify",
            &[("verify_report", &verify_report)],
        )
        .context("Verify: failed to load agent template")?;

        let prompt_path = self.run_dir.join("prompt_verify.md");
        fs::write(&prompt_path, &prompt).context("Verify: failed to write prompt_verify.md")?;

        agent::run(
            config.backend,
            &config.agent_cmd,
            &config.model,
            &prompt_path,
            &self.worktree,
            &self.run_dir,
        )
        .await
        .context("Verify: agent run failed")?;

        self.stage = Stage::Submit;
        Ok(true)
    }

    pub async fn do_submit(&mut self) -> Result<bool> {
        // Clean up stale git lock file from a potentially killed process.
        if let Ok(git_dir) = github::resolve_git_dir(&self.worktree).await {
            let git_lock: PathBuf = git_dir.join("index.lock");
            if git_lock.exists() {
                info!("removing stale git lock file: {}", git_lock.display());
                let _ = fs::remove_file(&git_lock);
            }
        }

        let commit_msg = format!("guild: implement issue #{}", self.issue_number);

        github::commit_all(&self.worktree, &commit_msg)
            .await
            .context("Submit: failed to commit")?;

        github::push_branch(&self.worktree, &self.branch_name)
            .await
            .context("Submit: failed to push branch")?;

        // Check if a PR already exists for this branch (e.g. from a previous
        // run that was interrupted after creating the PR).
        if let Some(existing) = github::find_pr_for_branch(&self.repo, &self.branch_name)
            .await
            .context("Submit: failed to check for existing PR")?
        {
            info!(
                "PR #{} already exists for branch {}, reusing",
                existing, self.branch_name
            );
            self.pr_number = Some(existing);
        } else {
            let pr_title = if self.issue_title.is_empty() {
                format!("guild: issue #{}", self.issue_number)
            } else {
                format!("guild: {} (#{})", self.issue_title, self.issue_number)
            };

            // Read the PR description written by the IMPLEMENT agent.
            let agent_description = read_file_or(&self.run_dir.join("pr_description.md"), "");

            let mut pr_body = format!("Closes #{}\n", self.issue_number);
            if !agent_description.is_empty() {
                pr_body.push('\n');
                pr_body.push_str(&agent_description);
                pr_body.push('\n');
            }
            pr_body.push_str("\n---\n*Generated by [guild](https://github.com/KaugaKauga/guild)*");

            let pr_number =
                github::create_pr(&self.repo, "main", &self.branch_name, &pr_title, &pr_body)
                    .await
                    .context("Submit: failed to create PR")?;

            info!("PR #{} created", pr_number);
            self.pr_number = Some(pr_number);
        }

        self.stage = Stage::Watch;
        Ok(true)
    }

    pub async fn do_watch(&mut self) -> Result<bool> {
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

        // React with 👀 on each @guild comment to acknowledge it.
        for c in &guild_comments {
            if let Some(ref node_id) = c.id {
                github::react_with_eyes(node_id).await;
            }
        }

        // Collect formal review bodies when changes are requested.
        let review_comments: Vec<&github::Review> = if status.review_decision == "CHANGES_REQUESTED"
        {
            status
                .reviews
                .iter()
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

        // Done condition: the PR has been merged.
        // We never mark Done just because checks are green -- the pipeline keeps
        // watching until a human merges (or closes) the PR.
        let pr_state = status.state.to_uppercase();

        if pr_state == "MERGED" {
            info!("PR #{} has been merged! Pipeline complete.", pr_number);
            self.stage = Stage::Done;
            return Ok(true);
        }

        // If the PR was closed without merging, mark the pipeline as failed
        // so it stops polling but can be investigated.
        if pr_state == "CLOSED" {
            info!(
                "PR #{} was closed without merging. Marking failed.",
                pr_number
            );
            self.stage = Stage::Failed("PR closed without merging".to_string());
            return Ok(true);
        }

        // Determine whether there are actual actionable blockers.
        let has_blockers =
            !failed_checks.is_empty() || !review_comments.is_empty() || !guild_comments.is_empty();

        // Check if blocker fingerprint changed.
        let changed = match &self.blocker_fingerprint {
            Some(prev) => prev != &fingerprint,
            None => {
                // First time in Watch — store baseline fingerprint.
                // Don't transition to Fix; wait for an actual change.
                self.blocker_fingerprint = Some(fingerprint);
                info!("Watch: baseline fingerprint stored for PR #{}", pr_number);
                return Ok(false);
            }
        };

        if changed && has_blockers {
            self.blocker_fingerprint = Some(fingerprint);

            // Write blocker report.
            let mut report = String::from("# Blocker Report\n\n");

            report.push_str("## Failed Checks\n");
            if failed_checks.is_empty() {
                report.push_str("- (none)\n");
            } else {
                // Fetch CI logs for failed checks (best-effort).
                let failed_logs = github::fetch_failed_check_logs(&self.repo, &failed_checks).await;

                for c in &failed_checks {
                    let conclusion = c.conclusion.as_deref().unwrap_or("unknown");
                    report.push_str(&format!("- **{}**: {}\n", c.name, conclusion));

                    // Include log output if available.
                    if let Some(log_entry) = failed_logs.iter().find(|l| l.name == c.name) {
                        report.push_str("\n<details><summary>CI Log</summary>\n\n```\n");
                        report.push_str(&log_entry.log);
                        report.push_str("\n```\n\n</details>\n\n");
                    }
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

        // Fingerprint changed but no actionable blockers — just update baseline.
        if changed {
            self.blocker_fingerprint = Some(fingerprint);
            info!(
                "Watch: fingerprint updated for PR #{} (no actionable blockers)",
                pr_number
            );
        }

        // No change -- will check again next poll.
        Ok(false)
    }

    pub async fn do_fix(&mut self, config: &Config) -> Result<bool> {
        // Clean up stale git lock file from a potentially killed process.
        if let Ok(git_dir) = github::resolve_git_dir(&self.worktree).await {
            let git_lock: PathBuf = git_dir.join("index.lock");
            if git_lock.exists() {
                info!("removing stale git lock file: {}", git_lock.display());
                let _ = fs::remove_file(&git_lock);
            }
        }

        let blocker_report = read_file_or(
            &self.run_dir.join("blocker_report.md"),
            "(no blocker report)",
        );
        let issue_body = read_file_or(&self.run_dir.join("issue_body.md"), "(no issue body)");

        // Re-read learnings from the worktree so we pick up anything the
        // IMPLEMENT or a previous FIX agent appended to .guild/learnings.md.
        let worktree_learnings = self.worktree.join(".guild").join("learnings.md");
        let learnings = if worktree_learnings.exists() {
            read_file_or(&worktree_learnings, "")
        } else {
            read_file_or(&self.run_dir.join("learnings.md"), "")
        };

        let prompt = load_agent_prompt(
            &config.agents_dir,
            "fix",
            &[
                ("blocker_report", &blocker_report),
                ("issue_body", &issue_body),
                ("learnings", &learnings),
            ],
        )
        .context("Fix: failed to load agent template")?;

        let prompt_path = self.run_dir.join("prompt_fix.md");
        fs::write(&prompt_path, &prompt).context("Fix: failed to write prompt_fix.md")?;

        agent::run(
            config.backend,
            &config.agent_cmd,
            &config.model,
            &prompt_path,
            &self.worktree,
            &self.run_dir,
        )
        .await
        .context("Fix: agent run failed")?;

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
                if ext == extension || path.to_string_lossy().ends_with(&format!(".{}", extension))
                {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_load_agent_prompt_substitutes_placeholders() {
        let dir = std::env::temp_dir().join("guild_test_agents");
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("test_stage.md"),
            "Hello {name}, your task is {task}.",
        )
        .unwrap();

        let result =
            load_agent_prompt(&dir, "test_stage", &[("name", "Alice"), ("task", "coding")])
                .unwrap();

        assert_eq!(result, "Hello Alice, your task is coding.");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_agent_prompt_missing_file() {
        let dir = std::env::temp_dir().join("guild_test_agents_missing");
        let result = load_agent_prompt(&dir, "nonexistent", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_agent_prompt_no_vars() {
        let dir = std::env::temp_dir().join("guild_test_agents_novars");
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("plain.md"), "No placeholders here.").unwrap();

        let result = load_agent_prompt(&dir, "plain", &[]).unwrap();
        assert_eq!(result, "No placeholders here.");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_agent_prompt_multiple_occurrences() {
        let dir = std::env::temp_dir().join("guild_test_agents_multi");
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("repeat.md"), "{x} and {x} again").unwrap();

        let result = load_agent_prompt(&dir, "repeat", &[("x", "val")]).unwrap();
        assert_eq!(result, "val and val again");
        let _ = fs::remove_dir_all(&dir);
    }
}
