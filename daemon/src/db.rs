//! SQLite-backed persistent state for guild pipelines.
//!
//! Two tables:
//!
//! - **pipelines** -- one row per active pipeline (keyed by issue number).
//! - **completed** -- permanent ledger of issues that reached Done.
//!
//! The Db handle is cheaply cloneable (Arc<Mutex<Connection>>) so it
//! can be shared across tokio tasks.  The mutex is only held for the duration
//! of each synchronous SQLite call -- never across an .await point.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tracing::{info, warn};

use crate::pipeline::{Pipeline, Stage};

/// Thread-safe, cloneable handle to the guild state database.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open (or create) the database at the given path and ensure the schema exists.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;

        conn.pragma_update(None, "journal_mode", "wal")
            .context("failed to enable WAL journal mode")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pipelines (
                issue_number        INTEGER PRIMARY KEY,
                repo                TEXT NOT NULL,
                stage               TEXT NOT NULL,
                run_dir             TEXT NOT NULL,
                worktree            TEXT NOT NULL,
                pr_number           INTEGER,
                blocker_fingerprint TEXT,
                branch_name         TEXT NOT NULL,
                issue_title         TEXT NOT NULL DEFAULT ''
            );

            CREATE TABLE IF NOT EXISTS completed (
                issue_number  INTEGER PRIMARY KEY,
                repo          TEXT NOT NULL,
                completed_at  TEXT NOT NULL,
                pr_number     INTEGER,
                run_dir       TEXT NOT NULL
            );",
        )
        .context("failed to create database tables")?;

        // --- Schema migrations ------------------------------------------------
        // CREATE TABLE IF NOT EXISTS won't alter an existing table, so we must
        // check for columns that were added after the initial schema and add
        // them with ALTER TABLE if missing.
        Self::migrate_add_column(
            &conn,
            "pipelines",
            "issue_title",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        // Add future column migrations here following the same pattern.
        // ----------------------------------------------------------------------

        info!(path = %path.display(), "database opened");

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Add a column to `table` if it does not already exist.
    ///
    /// Uses `PRAGMA table_info` to inspect the schema and only runs the
    /// `ALTER TABLE` when the column is genuinely missing.
    fn migrate_add_column(
        conn: &Connection,
        table: &str,
        column: &str,
        column_def: &str,
    ) -> Result<()> {
        let has_column: bool = {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({})", table))
                .with_context(|| format!("failed to inspect schema for {}", table))?;
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .collect();
            names.iter().any(|n| n == column)
        };

        if !has_column {
            let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, column_def);
            conn.execute(&sql, [])
                .with_context(|| format!("failed to add column {}.{}", table, column))?;
            info!(table, column, "migrated: added missing column");
        }

        Ok(())
    }

    /// Load every row from the pipelines table into a HashMap.
    pub fn get_all_active_pipelines(&self) -> Result<HashMap<u64, Pipeline>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT issue_number, repo, stage, run_dir, worktree,
                        pr_number, blocker_fingerprint, branch_name, issue_title
                 FROM   pipelines",
            )
            .context("failed to prepare pipeline query")?;

        let rows = stmt
            .query_map([], |row| {
                let stage_json: String = row.get(2)?;
                let stage: Stage = serde_json::from_str(&stage_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                Ok(Pipeline {
                    issue_number: row.get(0)?,
                    repo: row.get(1)?,
                    stage,
                    run_dir: PathBuf::from(row.get::<_, String>(3)?),
                    worktree: PathBuf::from(row.get::<_, String>(4)?),
                    pr_number: row.get(5)?,
                    blocker_fingerprint: row.get(6)?,
                    branch_name: row.get(7)?,
                    issue_title: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
                })
            })
            .context("failed to query pipelines")?;

        let mut map = HashMap::new();
        for row in rows {
            let p = row.context("failed to read pipeline row")?;
            map.insert(p.issue_number, p);
        }
        Ok(map)
    }

    /// Returns true if there is an active pipeline for the given issue number.
    pub fn has_pipeline(&self, issue_number: u64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pipelines WHERE issue_number = ?1",
                params![issue_number],
                |row| row.get(0),
            )
            .context("failed to check pipeline existence")?;
        Ok(count > 0)
    }

    /// Returns true if the given issue number is in the completed ledger.
    pub fn is_completed(&self, issue_number: u64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM completed WHERE issue_number = ?1",
                params![issue_number],
                |row| row.get(0),
            )
            .context("failed to check completed status")?;
        Ok(count > 0)
    }

    /// Insert or update a pipeline row.  Issue number is the primary key.
    pub fn upsert_pipeline(&self, p: &Pipeline) -> Result<()> {
        let stage_json =
            serde_json::to_string(&p.stage).context("failed to serialize pipeline stage")?;

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pipelines
                (issue_number, repo, stage, run_dir, worktree,
                 pr_number, blocker_fingerprint, branch_name, issue_title)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(issue_number) DO UPDATE SET
                stage               = excluded.stage,
                run_dir             = excluded.run_dir,
                worktree            = excluded.worktree,
                pr_number           = excluded.pr_number,
                blocker_fingerprint = excluded.blocker_fingerprint,
                branch_name         = excluded.branch_name,
                issue_title         = excluded.issue_title",
            params![
                p.issue_number,
                p.repo,
                stage_json,
                p.run_dir.to_string_lossy().as_ref(),
                p.worktree.to_string_lossy().as_ref(),
                p.pr_number,
                p.blocker_fingerprint,
                p.branch_name,
                p.issue_title,
            ],
        )
        .context("failed to upsert pipeline")?;

        Ok(())
    }

    /// Atomically move a pipeline from the active table into the completed
    /// ledger.  Both the DELETE and INSERT happen inside one transaction.
    pub fn complete_pipeline(&self, p: &Pipeline) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("failed to begin transaction")?;

        let now = chrono::Utc::now().to_rfc3339();

        tx.execute(
            "INSERT OR REPLACE INTO completed
                (issue_number, repo, completed_at, pr_number, run_dir)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                p.issue_number,
                p.repo,
                now,
                p.pr_number,
                p.run_dir.to_string_lossy().as_ref(),
            ],
        )
        .context("failed to insert into completed ledger")?;

        tx.execute(
            "DELETE FROM pipelines WHERE issue_number = ?1",
            params![p.issue_number],
        )
        .context("failed to delete completed pipeline")?;

        tx.commit()
            .context("failed to commit completion transaction")?;
        Ok(())
    }

    /// Remove a pipeline without recording it as completed.
    pub fn remove_pipeline(&self, issue_number: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM pipelines WHERE issue_number = ?1",
            params![issue_number],
        )
        .context("failed to remove pipeline")?;
        Ok(())
    }

    /// One-time migration from the legacy state.json file.
    ///
    /// If state.json exists in runs_dir, its pipelines are imported into the
    /// database inside a single transaction and the file is renamed to
    /// state.json.bak.
    pub fn migrate_from_state_json(&self, runs_dir: &Path) -> Result<()> {
        let state_path = runs_dir.join("state.json");
        if !state_path.exists() {
            return Ok(());
        }

        info!("found legacy state.json -- migrating to SQLite");

        let data = std::fs::read_to_string(&state_path)
            .context("failed to read state.json for migration")?;

        let pipelines: HashMap<u64, Pipeline> = match serde_json::from_str(&data) {
            Ok(p) => p,
            Err(e) => {
                warn!("state.json is corrupted, skipping migration: {:#}", e);
                let bad = runs_dir.join("state.json.corrupt");
                let _ = std::fs::rename(&state_path, &bad);
                return Ok(());
            }
        };

        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .context("failed to begin migration transaction")?;

        let now = chrono::Utc::now().to_rfc3339();
        let mut migrated = 0u64;

        for p in pipelines.values() {
            if p.is_done() {
                tx.execute(
                    "INSERT OR IGNORE INTO completed
                        (issue_number, repo, completed_at, pr_number, run_dir)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        p.issue_number,
                        p.repo,
                        now,
                        p.pr_number,
                        p.run_dir.to_string_lossy().as_ref(),
                    ],
                )
                .context("failed to migrate completed pipeline")?;
            } else {
                let stage_json = serde_json::to_string(&p.stage)
                    .context("failed to serialize stage for migration")?;
                tx.execute(
                    "INSERT OR IGNORE INTO pipelines
                        (issue_number, repo, stage, run_dir, worktree,
                         pr_number, blocker_fingerprint, branch_name, issue_title)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        p.issue_number,
                        p.repo,
                        stage_json,
                        p.run_dir.to_string_lossy().as_ref(),
                        p.worktree.to_string_lossy().as_ref(),
                        p.pr_number,
                        p.blocker_fingerprint,
                        p.branch_name,
                        p.issue_title,
                    ],
                )
                .context("failed to migrate active pipeline")?;
            }
            migrated += 1;
        }

        tx.commit()
            .context("failed to commit migration transaction")?;
        drop(conn);

        let bak = runs_dir.join("state.json.bak");
        std::fs::rename(&state_path, &bak)
            .context("failed to rename state.json to state.json.bak")?;

        info!(
            count = migrated,
            "migrated state.json to SQLite (original backed up to state.json.bak)"
        );
        Ok(())
    }
}
