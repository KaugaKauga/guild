mod copilot;
mod github;
mod pipeline;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

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
            runs_dir: PathBuf::from(&cli.runs_dir),
            max_concurrent: cli.max_concurrent,
        }
    }
}

/// Persist the current pipelines map to state.json inside runs_dir.
fn persist_state(pipelines: &HashMap<u64, pipeline::Pipeline>, runs_dir: &Path) -> Result<()> {
    let state_path = runs_dir.join("state.json");
    let json =
        serde_json::to_string_pretty(pipelines).context("failed to serialize pipeline state")?;
    std::fs::write(&state_path, json)
        .with_context(|| format!("failed to write state file at {}", state_path.display()))?;
    info!(path = %state_path.display(), "persisted pipeline state");
    Ok(())
}

/// Load previously-persisted pipelines from state.json inside runs_dir.
/// Returns an empty map when the file does not exist.
fn load_state(runs_dir: &Path) -> Result<HashMap<u64, pipeline::Pipeline>> {
    let state_path = runs_dir.join("state.json");
    if !state_path.exists() {
        info!("no existing state file found, starting fresh");
        return Ok(HashMap::new());
    }
    let data = std::fs::read_to_string(&state_path)
        .with_context(|| format!("failed to read state file at {}", state_path.display()))?;
    let pipelines: HashMap<u64, pipeline::Pipeline> =
        serde_json::from_str(&data).with_context(|| {
            format!(
                "failed to deserialize state file at {}",
                state_path.display()
            )
        })?;
    info!(
        count = pipelines.len(),
        path = %state_path.display(),
        "loaded persisted pipeline state"
    );
    Ok(pipelines)
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

    // --- load persisted state ----------------------------------------------
    let mut pipelines: HashMap<u64, pipeline::Pipeline> = load_state(&config.runs_dir)?;

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

        // Build a set of issue numbers currently active on GitHub so we can
        // clean up stale entries later.
        let active_on_github: HashSet<u64> = issues.iter().map(|i| i.number).collect();

        // 2. Create new pipelines for issues we have not seen yet.
        for issue in &issues {
            if !pipelines.contains_key(&issue.number) {
                info!(
                    issue = issue.number,
                    "new issue detected, creating pipeline"
                );
                let p =
                    pipeline::Pipeline::new(issue.number, config.repo.clone(), &config.runs_dir);
                pipelines.insert(issue.number, p);
            }
        }

        // 3. Advance active pipelines concurrently (up to max_concurrent).
        let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent));
        let mut join_set = tokio::task::JoinSet::new();

        // Keep a backup of every pipeline we send into a task so that if
        // the task panics we can recover the pipeline in its pre-advance
        // state instead of losing it entirely.
        let mut backups: HashMap<u64, pipeline::Pipeline> = HashMap::new();

        let keys: Vec<u64> = pipelines.keys().copied().collect();
        for key in keys {
            if let Some(p) = pipelines.remove(&key) {
                if p.is_done() || p.is_failed() {
                    pipelines.insert(key, p);
                    continue;
                }
                backups.insert(key, p.clone());
                let cfg = config.clone();
                let sem = Arc::clone(&semaphore);
                join_set.spawn(async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let mut pipeline = p;
                    // Drive the pipeline forward through all stages until it
                    // stalls (no progress), fails, or completes. This lets
                    // mechanical stages (Ingest, Understand, Submit) run
                    // back-to-back without waiting for the next poll cycle.
                    let mut last_result = Ok(false);
                    loop {
                        match pipeline.advance(&cfg).await {
                            Ok(true) => {
                                tracing::info!(issue = key, stage = ?pipeline.stage, "pipeline advanced");
                                last_result = Ok(true);
                                // Keep going — try the next stage immediately.
                                continue;
                            }
                            Ok(false) => {
                                // No progress (e.g. Watch with no changes). Stop.
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

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((key, pipeline, result)) => {
                    // Task completed normally — remove its backup.
                    backups.remove(&key);
                    match &result {
                        Ok(true) => info!(issue = key, "pipeline made progress"),
                        Ok(false) => {}
                        Err(e) => {
                            error!(issue = key, "pipeline advance error: {:#}", e);
                            warn!(issue = key, "pipeline marked as failed");
                        }
                    }
                    pipelines.insert(key, pipeline);
                }
                Err(e) => {
                    error!("pipeline task panicked: {:#}", e);
                    // The panicked task's key is unknown here, but its backup
                    // is still in the backups map and will be recovered below.
                }
            }
        }

        // Recover any pipelines whose tasks panicked. Their backups were
        // never removed, so whatever remains in `backups` needs to go back.
        for (key, backup) in backups {
            warn!(
                issue = key,
                stage = ?backup.stage,
                "recovering pipeline after task panic"
            );
            pipelines.insert(key, backup);
        }

        // 4. Clean up finished pipelines whose issues are no longer active
        //    on GitHub (label removed or issue closed). Done/Failed pipelines
        //    for issues that still carry the label are kept so they are never
        //    re-created from scratch.
        pipelines.retain(|issue_number, p| {
            if (p.is_done() || p.is_failed()) && !active_on_github.contains(issue_number) {
                info!(
                    issue = issue_number,
                    "pipeline finished and issue no longer active, cleaning up"
                );
                false
            } else {
                true
            }
        });

        // 5. Persist current state (includes Done/Failed pipelines).
        if let Err(e) = persist_state(&pipelines, &config.runs_dir) {
            error!("failed to persist state: {:#}", e);
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

    // Final state persist on exit.
    info!("persisting final state before exit");
    if let Err(e) = persist_state(&pipelines, &config.runs_dir) {
        error!("failed to persist state on shutdown: {:#}", e);
    }

    info!("guild daemon shut down cleanly");
    Ok(())
}
