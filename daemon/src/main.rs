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

/// Generate a brief status description for the TUI based on the pipeline stage.
fn stage_status_text(stage: &pipeline::Stage) -> String {
    use pipeline::Stage;
    match stage {
        Stage::Plan | Stage::Implement | Stage::Verify | Stage::Fix => "copilot running…".into(),
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
    // When TUI is active, log to a file so we don't corrupt the terminal.
    // When TUI is disabled (--no-tui), log to stderr as before.
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
        // Brief pause so the user can admire the art
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
    // Scan runs_dir for subdirectories not tracked by any active or completed
    // pipeline.  These may have been left behind by crashes or interrupted runs.
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

    // --- persistent concurrency primitives ---------------------------------
    // Pipeline tasks live across poll cycles.  The semaphore caps how many
    // advance concurrently, and the JoinSet holds the running tasks.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent));
    let mut join_set = tokio::task::JoinSet::<u64>::new();
    let mut running: HashSet<u64> = HashSet::new();

    // --- main loop ---------------------------------------------------------
    // The loop only polls GitHub and spawns / reaps pipeline tasks.  It never
    // blocks on pipeline advancement, so the poll cadence stays consistent.
    let mut cycle_count: u64 = 0;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            info!("shutdown flag set, breaking out of main loop");
            break;
        }

        cycle_count += 1;

        // 1. Fetch open issues that carry the target label.
        let issues = match github::fetch_labeled_issues(&config.repo, &config.label).await {
            Ok(issues) => {
                info!(count = issues.len(), "fetched labeled issues");
                issues
            }
            Err(e) => {
                error!("failed to fetch issues: {:#}", e);
                // Continue with empty list so we still reap tasks and refresh
                // the TUI — don't stall the whole loop.
                Vec::new()
            }
        };

        let active_on_github: HashSet<u64> = issues.iter().map(|i| i.number).collect();

        // 2. Create pipelines for issues we have not seen before.
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

        // 3. Load all active pipelines from the database.
        let mut pipelines = match db.get_all_active_pipelines() {
            Ok(p) => p,
            Err(e) => {
                error!("failed to load pipelines from database: {:#}", e);
                std::collections::HashMap::new()
            }
        };

        // 3a. Retry completing any Done pipelines left from a previous cycle.
        // 3b. Remove Failed pipelines whose issues are no longer on GitHub.
        // Skip pipelines that currently have a running background task.
        let mut housekeeping_keys: Vec<u64> = Vec::new();
        for (&issue_number, p) in &pipelines {
            if running.contains(&issue_number) {
                continue; // task is still running — don't interfere
            }
            if p.is_done() {
                if let Err(e) = db.complete_pipeline(p) {
                    error!(issue = issue_number, "failed to complete pipeline: {:#}", e);
                } else {
                    info!(issue = issue_number, "completed pipeline moved to ledger");
                    p.cleanup_run();
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

        // 4. Spawn background tasks for active pipelines that don't already
        //    have a running task.  Each task advances its pipeline through
        //    stages until it blocks (Watch returns Ok(false)) or errors out,
        //    then exits.  It will be re-spawned on the next poll cycle.
        let keys: Vec<u64> = pipelines.keys().copied().collect();
        for key in keys {
            if running.contains(&key) {
                continue;
            }
            if let Some(p) = pipelines.remove(&key) {
                if p.is_done() || p.is_failed() {
                    continue;
                }
                running.insert(key);
                let cfg = config.clone();
                let sem = Arc::clone(&semaphore);
                let db_handle = db.clone();
                let state_tx_inner = state_tx.clone();
                join_set.spawn(async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let mut pipeline = p;
                    loop {
                        match pipeline.advance(&cfg).await {
                            Ok(true) => {
                                if let Err(e) = db_handle.upsert_pipeline(&pipeline) {
                                    tracing::error!(
                                        issue = key,
                                        "failed to persist pipeline after advance: {:#}",
                                        e
                                    );
                                }
                                tracing::info!(
                                    issue = key,
                                    stage = %pipeline.stage,
                                    "pipeline advanced"
                                );

                                // Atomic TUI update — only touches this
                                // pipeline's entry, cannot clobber others.
                                state_tx_inner.send_modify(|state| {
                                    if let Some(snap) = state
                                        .pipelines
                                        .iter_mut()
                                        .find(|s| s.issue_number == pipeline.issue_number)
                                    {
                                        snap.stage = pipeline.stage.clone();
                                        snap.pr_number = pipeline.pr_number;
                                        snap.status_text = stage_status_text(&pipeline.stage);
                                    } else {
                                        state.pipelines.push(PipelineSnapshot {
                                            issue_number: pipeline.issue_number,
                                            issue_title: pipeline.issue_title.clone(),
                                            stage: pipeline.stage.clone(),
                                            branch_name: pipeline.branch_name.clone(),
                                            pr_number: pipeline.pr_number,
                                            status_text: stage_status_text(&pipeline.stage),
                                        });
                                    }
                                });

                                continue;
                            }
                            Ok(false) => break,
                            Err(e) => {
                                tracing::error!(issue = key, "pipeline advance error: {:#}", e);
                                break;
                            }
                        }
                    }

                    // Persist final state to DB.
                    if let Err(e) = db_handle.upsert_pipeline(&pipeline) {
                        tracing::error!(
                            issue = key,
                            "failed to persist pipeline final state: {:#}",
                            e
                        );
                    }

                    // Handle completion inside the task so the main poll
                    // loop is never blocked by cleanup / branch deletion.
                    if pipeline.is_done() {
                        tracing::info!(issue = key, "pipeline completed, recording in ledger");
                        if let Err(e) = db_handle.complete_pipeline(&pipeline) {
                            tracing::error!(issue = key, "failed to complete pipeline: {:#}", e);
                        } else {
                            github::delete_remote_branch(&pipeline.repo, &pipeline.branch_name)
                                .await;
                            pipeline.cleanup_run();
                        }
                    }

                    // Return issue number so the main loop can remove it
                    // from the running set.
                    key
                });
            }
        }

        // 5. Reap completed tasks (non-blocking).
        while let Some(result) = join_set.try_join_next() {
            match result {
                Ok(key) => {
                    running.remove(&key);
                    info!(issue = key, "pipeline task finished");
                }
                Err(e) => {
                    error!("pipeline task panicked: {:#}", e);
                }
            }
        }

        // 6. Full TUI state refresh from database.
        //    This runs every poll cycle so last_poll always tracks wall-clock
        //    time accurately and any in-flight state races are corrected.
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
                        status_text: stage_status_text(&p.stage),
                    })
                    .collect();
                state.last_poll = Some(now);
                state.cycle_count = cycle_count;
            });
        }

        // 7. Check for shutdown before sleeping.
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

// ---------------------------------------------------------------------------
// Orphan run directory cleanup
// ---------------------------------------------------------------------------

/// Remove run directories inside `runs_dir` that are not referenced by any
/// active or completed pipeline in the database.
///
/// Skips files (e.g. guild.db, guild.log) and only considers directories whose
/// names look like guild run dirs (contain a `-` to match the timestamp-slug
/// pattern).
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
