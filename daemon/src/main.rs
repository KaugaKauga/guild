mod banner;
mod copilot;
mod db;
mod github;
mod pipeline;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info};

use tui::{DaemonState, PipelineSnapshot};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Guild — an autonomous software factory.
#[derive(Parser, Debug)]
#[command(name = "guild", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the guild daemon with a live interactive dashboard.
    Start {
        /// GitHub repo in owner/repo format (positional or --repo)
        #[arg(value_name = "REPO")]
        repo: Option<String>,

        /// GitHub repo in owner/repo format
        #[arg(short, long)]
        repo_flag: Option<String>,

        /// Issue label to filter on
        #[arg(short, long, default_value = "guild")]
        label: String,

        /// Seconds between polling cycles
        #[arg(short, long, default_value_t = 30)]
        poll_interval: u64,

        /// Name or path of the copilot CLI binary
        #[arg(long, default_value = "copilot")]
        copilot_cmd: String,

        /// AI model to use (e.g. claude-opus-4.6, gpt-5.2)
        #[arg(short = 'm', long, default_value = "claude-opus-4.6")]
        model: String,

        /// Directory where run artifacts are stored
        #[arg(long, default_value = "./runs")]
        runs_dir: String,

        /// Maximum number of pipelines to advance concurrently
        #[arg(short = 'c', long, default_value_t = 5)]
        max_concurrent: usize,

        /// Directory containing agent prompt templates
        #[arg(long, default_value = "./agents")]
        agents_dir: String,

        /// Disable the TUI and use plain log output
        #[arg(long)]
        no_tui: bool,
    },

    /// Show current pipeline status and exit.
    Status {
        /// Directory where run artifacts are stored
        #[arg(long, default_value = "./runs")]
        runs_dir: String,
    },
}

/// Runtime configuration derived from CLI arguments.
#[derive(Clone, Debug)]
pub struct Config {
    pub repo: String,
    pub label: String,
    pub poll_interval: u64,
    pub copilot_cmd: String,
    pub model: String,
    pub runs_dir: PathBuf,
    pub max_concurrent: usize,
    pub agents_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            repo,
            repo_flag,
            label,
            poll_interval,
            copilot_cmd,
            model,
            runs_dir,
            max_concurrent,
            agents_dir,
            no_tui,
        } => {
            let repo = repo
                .or(repo_flag)
                .expect("repo is required: guild start <owner/repo>");

            let config = Config {
                repo,
                label,
                poll_interval,
                copilot_cmd,
                model,
                runs_dir: PathBuf::from(&runs_dir),
                max_concurrent,
                agents_dir: PathBuf::from(&agents_dir).canonicalize().with_context(|| {
                    format!(
                        "agents directory not found: {} (pass --agents-dir with a valid path)",
                        agents_dir
                    )
                })?,
            };

            run_start(config, no_tui).await
        }
        Commands::Status { runs_dir } => run_status(&runs_dir),
    }
}

// ---------------------------------------------------------------------------
// `guild status` — one-shot status display
// ---------------------------------------------------------------------------

fn run_status(runs_dir: &str) -> Result<()> {
    let db_path = PathBuf::from(runs_dir).join("guild.db");
    if !db_path.exists() {
        println!("No guild database found at {}", db_path.display());
        println!("Start the daemon first with: guild start <owner/repo>");
        return Ok(());
    }

    let db = db::Db::open(&db_path)?;
    let pipelines = db.get_all_active_pipelines()?;

    if pipelines.is_empty() {
        println!("No active pipelines.");
    } else {
        println!(
            "{:<8} {:<32} {:<14} {:<12} Branch",
            "Issue", "Title", "Stage", "Progress"
        );
        println!("{}", "─".repeat(90));
        for p in pipelines.values() {
            let ordinal = p.stage.ordinal();
            let total = pipeline::Stage::total_stages();
            let title = if p.issue_title.chars().count() > 30 {
                let t: String = p.issue_title.chars().take(30).collect();
                format!("{}…", t)
            } else {
                p.issue_title.clone()
            };
            println!(
                "#{:<7} {:<32} {:<14} {}/{:<10} {}",
                p.issue_number, title, p.stage, ordinal, total, p.branch_name
            );
        }
    }
    Ok(())
}
// ---------------------------------------------------------------------------
// `guild start` — main daemon with TUI
// ---------------------------------------------------------------------------

/// Generate a brief status description for the TUI based on the pipeline
/// stage and whether an agent task is currently running for it.
fn stage_status_text(stage: &pipeline::Stage, has_running_task: bool) -> String {
    use pipeline::Stage;
    if has_running_task && stage.needs_agent() {
        return "copilot running…".into();
    }
    match stage {
        Stage::Plan | Stage::Implement | Stage::Verify | Stage::Fix => {
            if has_running_task {
                "copilot running…".into()
            } else {
                "waiting for slot…".into()
            }
        }
        Stage::Ingest => "fetching issue…".into(),
        Stage::Understand => "analyzing…".into(),
        Stage::Submit => "pushing PR…".into(),
        Stage::Watch => "watching CI…".into(),
        Stage::Done => "complete".into(),
        Stage::Failed(_) => "failed".into(),
    }
}

async fn run_start(config: Config, no_tui: bool) -> Result<()> {
    // --- ensure runs dir ---------------------------------------------------
    std::fs::create_dir_all(&config.runs_dir).with_context(|| {
        format!(
            "failed to create runs directory at {}",
            config.runs_dir.display()
        )
    })?;

    // --- tracing -----------------------------------------------------------
    if no_tui {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("guild=info")),
            )
            .init();
    } else {
        let log_path = config.runs_dir.join("guild.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("failed to open log file at {}", log_path.display()))?;

        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("guild=info")),
            )
            .with_writer(std::sync::Mutex::new(log_file))
            .with_ansi(false)
            .init();
    }

    // --- print banner (before TUI takes over the screen) -------------------
    if !no_tui {
        banner::print_banner();
        println!("  Starting daemon for {}...\n", config.repo);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    info!("=======================================================");
    info!("  Guild daemon starting");
    info!("  repo           : {}", config.repo);
    info!("  label          : {}", config.label);
    info!("  poll_interval  : {}s", config.poll_interval);
    info!("  copilot_cmd    : {}", config.copilot_cmd);
    info!("  model          : {}", config.model);
    info!("  runs_dir       : {}", config.runs_dir.display());
    info!("  max_concurrent : {}", config.max_concurrent);
    info!("=======================================================");

    // --- open database -----------------------------------------------------
    let db = db::Db::open(&config.runs_dir.join("guild.db"))?;

    // --- migrate legacy state.json if present ------------------------------
    db.migrate_from_state_json(&config.runs_dir)?;

    // --- log current state -------------------------------------------------
    let existing = db.get_all_active_pipelines()?;
    info!(active = existing.len(), "active pipelines in database");
    drop(existing);

    // --- clean up orphaned run directories --------------------------------
    cleanup_orphan_run_dirs(&config.runs_dir, &db);

    // --- graceful shutdown flag --------------------------------------------
    let shutdown = Arc::new(AtomicBool::new(false));

    // --- TUI state channel -------------------------------------------------
    let initial_state = DaemonState {
        pipelines: Vec::new(),
        last_poll: None,
        cycle_count: 0,
        repo: config.repo.clone(),
        poll_interval: config.poll_interval,
    };
    let (state_tx, state_rx) = tokio::sync::watch::channel(initial_state);

    // --- spawn TUI (or ctrl-c handler for --no-tui) ------------------------
    if no_tui {
        let shutdown_hook = Arc::clone(&shutdown);
        tokio::spawn(async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!("failed to listen for ctrl-c: {}", e);
            }
            info!("received ctrl-c, shutting down after current cycle");
            shutdown_hook.store(true, Ordering::SeqCst);
        });
    } else {
        let shutdown_tui = Arc::clone(&shutdown);
        tokio::spawn(async move {
            if let Err(e) = tui::run_tui(state_rx, shutdown_tui).await {
                eprintln!("TUI error: {}", e);
            }
        });
    }

    // --- concurrency primitives --------------------------------------------
    // The semaphore only gates copilot (agent) tasks.  Orchestrator stages
    // (Ingest, Understand, Submit, Watch) run without a permit.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent));

    // JoinSet for background tasks.  Each task does exactly ONE stage and
    // exits, returning its issue number so the orchestrator can advance it.
    let mut join_set = tokio::task::JoinSet::<(u64, std::result::Result<bool, String>)>::new();

    // Set of issue numbers that currently have a spawned task in the JoinSet.
    let mut running: HashSet<u64> = HashSet::new();

    // --- main orchestrator loop --------------------------------------------
    let mut cycle_count: u64 = 0;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            info!("shutdown flag set, breaking out of main loop");
            break;
        }

        cycle_count += 1;

        // -- 1. Reap completed tasks (before anything else) -----------------
        // Tasks that finished since the last tick get processed first so
        // the orchestrator has an up-to-date picture of what is running.
        while let Some(result) = join_set.try_join_next() {
            match result {
                Ok((issue_number, task_result)) => {
                    running.remove(&issue_number);
                    match task_result {
                        Ok(true) => {
                            info!(issue = issue_number, "task completed, stage advanced");
                        }
                        Ok(false) => {
                            info!(issue = issue_number, "task completed, no progress");
                        }
                        Err(ref msg) => {
                            error!(issue = issue_number, "task failed: {}", msg);
                        }
                    }
                }
                Err(e) => {
                    // Task panicked.  We lost the issue number.
                    // The pipeline is still in the DB and will be retried
                    // next cycle (it won't be in `running` because the
                    // JoinSet entry is gone).
                    error!("pipeline task panicked: {:#}", e);
                }
            }
        }

        // -- 2. Fetch open issues with the target label ---------------------
        let issues = match github::fetch_labeled_issues(&config.repo, &config.label).await {
            Ok(issues) => {
                info!(count = issues.len(), "fetched labeled issues");
                issues
            }
            Err(e) => {
                error!("failed to fetch issues: {:#}", e);
                Vec::new()
            }
        };

        let active_on_github: HashSet<u64> = issues.iter().map(|i| i.number).collect();

        // -- 3. Create pipelines for new issues -----------------------------
        for issue in &issues {
            let already_known = db.has_pipeline(issue.number).unwrap_or(false)
                || db.is_completed(issue.number).unwrap_or(false);
            if already_known {
                continue;
            }

            info!(
                issue = issue.number,
                "new issue detected, creating pipeline"
            );
            let p = pipeline::Pipeline::new(issue.number, config.repo.clone(), &config.runs_dir);
            if let Err(e) = db.upsert_pipeline(&p) {
                error!(
                    issue = issue.number,
                    "failed to persist new pipeline: {:#}", e
                );
            }
        }

        // -- 4. Load all active pipelines from the database -----------------
        let mut pipelines = match db.get_all_active_pipelines() {
            Ok(p) => p,
            Err(e) => {
                error!("failed to load pipelines from database: {:#}", e);
                std::collections::HashMap::new()
            }
        };

        // -- 5. Housekeeping: complete Done, remove stale Failed ------------
        let mut housekeeping_keys: Vec<u64> = Vec::new();
        for (&issue_number, p) in &pipelines {
            if running.contains(&issue_number) {
                continue;
            }
            if p.is_done() {
                if let Err(e) = db.complete_pipeline(p) {
                    error!(issue = issue_number, "failed to complete pipeline: {:#}", e);
                } else {
                    info!(issue = issue_number, "completed pipeline moved to ledger");
                    // Spawn a lightweight task for cleanup so we don't block
                    // the orchestrator on branch deletion / disk I/O.
                    let repo = p.repo.clone();
                    let branch = p.branch_name.clone();
                    let worktree = p.worktree.clone();
                    let run_dir = p.run_dir.clone();
                    let inum = p.issue_number;
                    tokio::spawn(async move {
                        github::delete_remote_branch(&repo, &branch).await;
                        if worktree.exists() {
                            if let Err(e) = std::fs::remove_dir_all(&worktree) {
                                tracing::warn!(issue = inum, "cleanup: remove worktree: {:#}", e);
                            }
                        }
                        if run_dir.exists() {
                            if let Err(e) = std::fs::remove_dir_all(&run_dir) {
                                tracing::warn!(issue = inum, "cleanup: remove run_dir: {:#}", e);
                            }
                        }
                    });
                    housekeeping_keys.push(issue_number);
                }
            } else if p.is_failed() && !active_on_github.contains(&issue_number) {
                info!(
                    issue = issue_number,
                    "removing failed pipeline for inactive issue"
                );
                if let Err(e) = db.remove_pipeline(issue_number) {
                    error!(issue = issue_number, "failed to remove pipeline: {:#}", e);
                } else {
                    housekeeping_keys.push(issue_number);
                }
            }
        }
        for key in housekeeping_keys {
            pipelines.remove(&key);
        }

        // -- 6. Process each active pipeline --------------------------------
        // Split into two passes so that inline WATCH processing is
        // reflected in the TUI before we spawn long-running agent tasks.
        //
        // Pass 1 (inline): WATCH pipelines — quick GH API calls.
        // Pass 2 (spawned): agent & orchestrator stages.
        let keys: Vec<u64> = pipelines.keys().copied().collect();

        // --- Pass 1: WATCH (inline) ----------------------------------------
        for &key in &keys {
            if running.contains(&key) {
                continue;
            }
            let is_watch = pipelines
                .get(&key)
                .is_some_and(|p| matches!(p.stage, pipeline::Stage::Watch));
            if !is_watch {
                continue;
            }

            let mut p = match pipelines.remove(&key) {
                Some(p) => p,
                None => continue,
            };

            match p.do_watch().await {
                Ok(true) => {
                    // Stage changed (Fix, Done, or Failed).
                    if let Err(e) = db.upsert_pipeline(&p) {
                        error!(issue = key, "failed to persist Watch transition: {:#}", e);
                    }
                    info!(issue = key, stage = %p.stage, "Watch: stage transitioned");
                }
                Ok(false) => {
                    // No change.  Persist so the fingerprint is saved.
                    if let Err(e) = db.upsert_pipeline(&p) {
                        error!(issue = key, "failed to persist Watch state: {:#}", e);
                    }
                }
                Err(e) => {
                    error!(issue = key, "Watch check failed: {:#}", e);
                }
            }
        }

        // --- Mid-cycle TUI refresh after WATCH processing ------------------
        // This ensures all WATCH-stage pipelines are visible immediately,
        // before long-running agent tasks are spawned.
        if let Ok(all) = db.get_all_active_pipelines() {
            let now = chrono::Utc::now();
            state_tx.send_modify(|state| {
                state.pipelines = all
                    .values()
                    .map(|p| PipelineSnapshot {
                        issue_number: p.issue_number,
                        issue_title: p.issue_title.clone(),
                        stage: p.stage.clone(),
                        branch_name: p.branch_name.clone(),
                        pr_number: p.pr_number,
                        status_text: stage_status_text(&p.stage, running.contains(&p.issue_number)),
                    })
                    .collect();
                state.last_poll = Some(now);
                state.cycle_count = cycle_count;
            });
        }

        // --- Pass 2: Agent & Orchestrator stages (spawned) -----------------
        for key in keys {
            if running.contains(&key) {
                continue;
            }

            let p = match pipelines.remove(&key) {
                Some(p) => p,
                None => continue,
            };

            if p.is_done() || p.is_failed() {
                continue;
            }

            match &p.stage {
                // Watch already handled in Pass 1.
                pipeline::Stage::Watch => {}

                // -- Agent stages: spawn copilot task with semaphore ---------
                pipeline::Stage::Plan
                | pipeline::Stage::Implement
                | pipeline::Stage::Verify
                | pipeline::Stage::Fix => {
                    running.insert(key);
                    let cfg = config.clone();
                    let sem = Arc::clone(&semaphore);
                    let db_handle = db.clone();
                    join_set.spawn(async move {
                        let _permit = sem.acquire().await.expect("semaphore closed");
                        let mut pipeline = p;
                        let stage_name = format!("{}", pipeline.stage);
                        info!(issue = key, stage = %stage_name, "agent task started");

                        let result = pipeline.advance(&cfg).await;

                        // Always persist state to DB, even on error.
                        if let Err(e) = db_handle.upsert_pipeline(&pipeline) {
                            tracing::error!(
                                issue = key,
                                "failed to persist pipeline after {}: {:#}",
                                stage_name,
                                e
                            );
                        }

                        match result {
                            Ok(advanced) => {
                                info!(
                                    issue = key,
                                    stage = %pipeline.stage,
                                    advanced,
                                    "agent task finished"
                                );
                                (key, Ok(advanced))
                            }
                            Err(e) => {
                                let msg = format!("{} failed: {:#}", stage_name, e);
                                error!(issue = key, "{}", msg);
                                (key, Err(msg))
                            }
                        }
                    });
                }

                // -- Orchestrator stages: spawn lightweight task -------------
                // No semaphore needed — these don't run copilot.
                pipeline::Stage::Ingest | pipeline::Stage::Understand | pipeline::Stage::Submit => {
                    running.insert(key);
                    let cfg = config.clone();
                    let db_handle = db.clone();
                    join_set.spawn(async move {
                        let mut pipeline = p;
                        let stage_name = format!("{}", pipeline.stage);

                        let result = pipeline.advance(&cfg).await;

                        if let Err(e) = db_handle.upsert_pipeline(&pipeline) {
                            tracing::error!(
                                issue = key,
                                "failed to persist pipeline after {}: {:#}",
                                stage_name,
                                e
                            );
                        }

                        match result {
                            Ok(advanced) => {
                                info!(
                                    issue = key,
                                    stage = %pipeline.stage,
                                    advanced,
                                    "orchestrator task finished"
                                );
                                (key, Ok(advanced))
                            }
                            Err(e) => {
                                let msg = format!("{} failed: {:#}", stage_name, e);
                                error!(issue = key, "{}", msg);
                                (key, Err(msg))
                            }
                        }
                    });
                }

                // Done/Failed already handled in housekeeping above.
                pipeline::Stage::Done | pipeline::Stage::Failed(_) => {}
            }
        }

        // -- 7. TUI refresh from database (single source of truth) ----------
        // Final refresh after all spawned tasks have been kicked off,
        // so the `running` set is up-to-date for status text.
        if let Ok(all) = db.get_all_active_pipelines() {
            let now = chrono::Utc::now();
            state_tx.send_modify(|state| {
                state.pipelines = all
                    .values()
                    .map(|p| PipelineSnapshot {
                        issue_number: p.issue_number,
                        issue_title: p.issue_title.clone(),
                        stage: p.stage.clone(),
                        branch_name: p.branch_name.clone(),
                        pr_number: p.pr_number,
                        status_text: stage_status_text(&p.stage, running.contains(&p.issue_number)),
                    })
                    .collect();
                state.last_poll = Some(now);
                state.cycle_count = cycle_count;
            });
        }

        // -- 8. Check for shutdown before sleeping --------------------------
        if shutdown.load(Ordering::SeqCst) {
            info!("shutdown flag set, breaking out of main loop");
            break;
        }

        info!(
            seconds = config.poll_interval,
            running = running.len(),
            "sleeping until next poll cycle"
        );
        tokio::time::sleep(std::time::Duration::from_secs(config.poll_interval)).await;
    }

    info!("guild daemon shut down cleanly");
    Ok(())
}
fn cleanup_orphan_run_dirs(runs_dir: &std::path::Path, db: &db::Db) {
    let tracked = match db.all_tracked_run_dirs() {
        Ok(t) => t,
        Err(e) => {
            error!(
                "failed to query tracked run dirs, skipping orphan cleanup: {:#}",
                e
            );
            return;
        }
    };

    let entries = match std::fs::read_dir(runs_dir) {
        Ok(e) => e,
        Err(e) => {
            error!("failed to read runs_dir for orphan cleanup: {:#}", e);
            return;
        }
    };

    let mut removed = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Only consider directories that look like run dirs (timestamp-slug pattern).
        if !dir_name.contains('-') {
            continue;
        }

        // Check if this directory is tracked (by absolute or relative path).
        let abs_str = path.to_string_lossy().to_string();
        let is_tracked = tracked.contains(&abs_str)
            || tracked.iter().any(|t| {
                // The DB may store relative or absolute paths; check if either
                // matches the directory.
                let t_path = std::path::Path::new(t);
                t_path == path || t_path.file_name() == path.file_name()
            });

        if !is_tracked {
            info!(path = %path.display(), "removing orphaned run directory");
            if let Err(e) = std::fs::remove_dir_all(&path) {
                error!(path = %path.display(), "failed to remove orphan dir: {:#}", e);
            } else {
                removed += 1;
            }
        }
    }

    if removed > 0 {
        info!(count = removed, "cleaned up orphaned run directories");
    }
}
