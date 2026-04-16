//! Copilot CLI integration.
//!
//! Spawns the copilot CLI binary to handle intelligent stages (plan, implement,
//! verify, fix). Uses non-interactive mode (-p) with full permissions (--yolo)
//! and autonomous operation (--no-ask-user).
//!
//! The copilot process is expected to:
//! - Receive the prompt content via -p (non-interactive mode)
//! - Have full tool permissions via --yolo
//! - Operate on files in the working directory
//! - Exit 0 on success, non-zero on failure
//!
//! All intelligence lives in the copilot process itself. This module is
//! intentionally thin glue.

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tracing::{error, info};

/// Run the copilot CLI with the given prompt file in the given working directory.
///
/// Reads the prompt file, then invokes copilot in non-interactive mode with
/// full permissions:
///   copilot -p <content> --yolo --no-ask-user
///
/// stdout and stderr are inherited so copilot output streams to our terminal.
/// stdin is nulled out -- no interactive input.
pub async fn run_copilot(copilot_cmd: &str, prompt_file: &Path, work_dir: &Path) -> Result<()> {
    // Read the prompt file content to pass via -p.
    let content = tokio::fs::read_to_string(prompt_file)
        .await
        .with_context(|| format!("failed to read prompt file: {}", prompt_file.display()))?;

    info!(
        cmd = copilot_cmd,
        prompt = %prompt_file.display(),
        dir = %work_dir.display(),
        "spawning copilot (non-interactive, yolo)"
    );

    let result = tokio::process::Command::new(copilot_cmd)
        .arg("-p")
        .arg(&content)
        .arg("--yolo")
        .arg("--no-ask-user")
        .current_dir(work_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null())
        .spawn();

    let mut child = match result {
        Ok(child) => child,
        Err(e) => {
            error!(cmd = copilot_cmd, "failed to spawn copilot process: {}", e);
            bail!(
                "failed to spawn '{}': {}. Check your --copilot-cmd flag.",
                copilot_cmd,
                e
            );
        }
    };

    let status = child
        .wait()
        .await
        .context("failed to wait on copilot process")?;

    if status.success() {
        info!("copilot completed successfully");
        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);
        error!(code, "copilot exited with non-zero status");
        bail!("copilot exited with code {}", code)
    }
}
