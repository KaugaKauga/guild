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
        println!("{:<8} {:<14} {:<12} Branch", "Issue", "Stage", "Progress");
        println!("{}", "─".repeat(60));
        for p in pipelines.values() {
            let ordinal = p.stage.ordinal();
            let total = pipeline::Stage::total_stages();
            println!(
                "#{:<7} {:<14} {}/{:<10} {}",
                p.issue_number, p.stage, ordinal, total, p.branch_name
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `guild start` — main daemon with TUI
// ---------------------------------------------------------------------------

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

    // --- main loop ---------------------------------------------------------
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
                tokio::time::sleep(std::time::Duration::from_secs(config.poll_interval)).await;
                continue;
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
                tokio::time::sleep(std::time::Duration::from_secs(config.poll_interval)).await;
                continue;
            }
        };

        // 3a. Retry completing any Done pipelines left from a previous cycle.
        // 3b. Remove Failed pipelines whose issues are no longer on GitHub.
        let mut housekeeping_keys: Vec<u64> = Vec::new();
        for (&issue_number, p) in &pipelines {
            if p.is_done() {
                if let Err(e) = db.complete_pipeline(p) {
                    error!(issue = issue_number, "failed to complete pipeline: {:#}", e);
                } else {
                    info!(issue = issue_number, "completed pipeline moved to ledger");
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

        // --- Update TUI state before advancing pipelines ---
        let now = chrono::Utc::now();
        let _ = state_tx.send(DaemonState {
            pipelines: pipelines
                .values()
                .map(|p| PipelineSnapshot {
                    issue_number: p.issue_number,
                    stage: p.stage.clone(),
                    branch_name: p.branch_name.clone(),
                    pr_number: p.pr_number,
                })
                .collect(),
            last_poll: Some(now),
            cycle_count,
            repo: config.repo.clone(),
            poll_interval: config.poll_interval,
        });

        // 4. Advance active pipelines concurrently (up to max_concurrent).
        let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent));
        let mut join_set = tokio::task::JoinSet::new();

        let keys: Vec<u64> = pipelines.keys().copied().collect();
        for key in keys {
            if let Some(p) = pipelines.remove(&key) {
                if p.is_done() || p.is_failed() {
                    continue;
                }
                let cfg = config.clone();
                let sem = Arc::clone(&semaphore);
                let db_handle = db.clone();
                let state_tx_inner = state_tx.clone();
                join_set.spawn(async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let mut pipeline = p;
                    let mut last_result = Ok(false);
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

                                // Send a state update after each stage change
                                // (read current state and update this pipeline's entry)
                                let mut current = state_tx_inner.borrow().clone();
                                if let Some(snap) = current
                                    .pipelines
                                    .iter_mut()
                                    .find(|s| s.issue_number == pipeline.issue_number)
                                {
                                    snap.stage = pipeline.stage.clone();
                                    snap.pr_number = pipeline.pr_number;
                                } else {
                                    current.pipelines.push(PipelineSnapshot {
                                        issue_number: pipeline.issue_number,
                                        stage: pipeline.stage.clone(),
                                        branch_name: pipeline.branch_name.clone(),
                                        pr_number: pipeline.pr_number,
                                    });
                                }
                                let _ = state_tx_inner.send(current);

                                last_result = Ok(true);
                                continue;
                            }
                            Ok(false) => {
                                last_result = Ok(last_result.unwrap_or(false));
                                break;
                            }
                            Err(e) => {
                                last_result = Err(e);
                                break;
                            }
                        }
                    }
                    (key, pipeline, last_result)
                });
            }
        }

        // 5. Collect results.
        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((key, pipeline, result)) => {
                    match &result {
                        Ok(true) => info!(issue = key, "pipeline made progress"),
                        Ok(false) => {}
                        Err(e) => {
                            error!(issue = key, "pipeline advance error: {:#}", e);
                        }
                    }
                    if let Err(e) = db.upsert_pipeline(&pipeline) {
                        error!(issue = key, "failed to persist pipeline state: {:#}", e);
                    }
                    if pipeline.is_done() {
                        info!(issue = key, "pipeline completed, recording in ledger");
                        if let Err(e) = db.complete_pipeline(&pipeline) {
                            error!(issue = key, "failed to complete pipeline: {:#}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("pipeline task panicked: {:#}", e);
                }
            }
        }

        // --- Update TUI state after advancing ---
        if let Ok(all) = db.get_all_active_pipelines() {
            let _ = state_tx.send(DaemonState {
                pipelines: all
                    .values()
                    .map(|p| PipelineSnapshot {
                        issue_number: p.issue_number,
                        stage: p.stage.clone(),
                        branch_name: p.branch_name.clone(),
                        pr_number: p.pr_number,
                    })
                    .collect(),
                last_poll: Some(chrono::Utc::now()),
                cycle_count,
                repo: config.repo.clone(),
                poll_interval: config.poll_interval,
            });
        }

        // 6. Check for shutdown before sleeping.
        if shutdown.load(Ordering::SeqCst) {
            info!("shutdown flag set, breaking out of main loop");
            break;
        }

        info!(
            seconds = config.poll_interval,
            "sleeping until next poll cycle"
        );
        tokio::time::sleep(std::time::Duration::from_secs(config.poll_interval)).await;
    }

    info!("guild daemon shut down cleanly");
    Ok(())
}
