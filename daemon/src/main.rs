mod copilot;
mod db;
mod github;
mod pipeline;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info};

/// Guild -- an autonomous software factory daemon.
/// Monitors a GitHub repo for labeled issues and drives them through
/// a pipeline that ends with a pull request.
#[derive(Parser, Debug)]
#[command(name = "guild", version, about)]
struct Cli {
    /// GitHub repo in owner/repo format
    #[arg(short, long)]
    repo: String,

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

impl Config {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            repo: cli.repo.clone(),
            label: cli.label.clone(),
            poll_interval: cli.poll_interval,
            copilot_cmd: cli.copilot_cmd.clone(),
            model: cli.model.clone(),
            runs_dir: PathBuf::from(&cli.runs_dir),
            max_concurrent: cli.max_concurrent,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // --- tracing -----------------------------------------------------------
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("guild=info")),
        )
        .init();

    // --- CLI / config ------------------------------------------------------
    let cli = Cli::parse();
    let config = Config::from_cli(&cli);

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

    // --- ensure runs dir ---------------------------------------------------
    std::fs::create_dir_all(&config.runs_dir).with_context(|| {
        format!(
            "failed to create runs directory at {}",
            config.runs_dir.display()
        )
    })?;

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
    let shutdown_hook = Arc::clone(&shutdown);
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("failed to listen for ctrl-c: {}", e);
        }
        info!("received ctrl-c, shutting down after current cycle");
        shutdown_hook.store(true, Ordering::SeqCst);
    });

    // --- main loop ---------------------------------------------------------
    loop {
        if shutdown.load(Ordering::SeqCst) {
            info!("shutdown flag set, breaking out of main loop");
            break;
        }

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
        //    Check BOTH the active-pipelines table and the completed ledger
        //    so we never re-run work that already finished.
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

        // 3a. Retry completing any Done pipelines left from a previous cycle
        //     where complete_pipeline failed.
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
                join_set.spawn(async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let mut pipeline = p;
                    // Drive the pipeline forward through all stages until it
                    // stalls (no progress), fails, or completes.  Persist to
                    // the database after every stage transition so progress
                    // survives crashes.
                    let mut last_result = Ok(false);
                    loop {
                        match pipeline.advance(&cfg).await {
                            Ok(true) => {
                                // Persist immediately after each stage change.
                                if let Err(e) = db_handle.upsert_pipeline(&pipeline) {
                                    tracing::error!(
                                        issue = key,
                                        "failed to persist pipeline after advance: {:#}",
                                        e
                                    );
                                }
                                tracing::info!(
                                    issue = key,
                                    stage = ?pipeline.stage,
                                    "pipeline advanced"
                                );
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
                    // Final persist (covers errors that happened after the
                    // last in-loop persist).
                    if let Err(e) = db.upsert_pipeline(&pipeline) {
                        error!(issue = key, "failed to persist pipeline state: {:#}", e);
                    }
                    // Move completed pipelines to the permanent ledger.
                    if pipeline.is_done() {
                        info!(issue = key, "pipeline completed, recording in ledger");
                        if let Err(e) = db.complete_pipeline(&pipeline) {
                            error!(issue = key, "failed to complete pipeline: {:#}", e);
                        }
                    }
                }
                Err(e) => {
                    // Task panicked.  The pipeline's last persisted state is
                    // safe in the database; it will be retried next cycle.
                    error!("pipeline task panicked: {:#}", e);
                }
            }
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
