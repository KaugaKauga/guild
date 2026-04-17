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

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tracing::{error, info, warn};

/// Run the copilot CLI with the given prompt file in the given working directory.
///
/// Reads the prompt file, then invokes copilot in non-interactive mode with
/// full permissions:
///   copilot -p <content> --yolo --no-ask-user
///
/// Output is redirected to log files under `run_dir` to avoid corrupting the
/// TUI dashboard. Log files are named after the prompt file stem, e.g.
/// `copilot_plan.log` for `prompt_plan.md`.
///
/// stdin is nulled out -- no interactive input.
///
/// The child process is started in its own session (setsid) so that it has
/// no controlling terminal.  This prevents interactive CLI tools (e.g. Claude
/// Code) from writing directly to /dev/tty, which would corrupt the ratatui
/// TUI running in the parent.
pub async fn run_copilot(
    copilot_cmd: &str,
    model: &str,
    prompt_file: &Path,
    work_dir: &Path,
    run_dir: &Path,
) -> Result<()> {
    // Read the prompt file content to pass via -p.
    let content = tokio::fs::read_to_string(prompt_file)
        .await
        .with_context(|| format!("failed to read prompt file: {}", prompt_file.display()))?;

    // Derive log file name from prompt file stem (e.g. prompt_plan -> copilot_plan.log).
    let stage_name = prompt_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("copilot")
        .strip_prefix("prompt_")
        .unwrap_or("unknown");
    let log_name = format!("copilot_{stage_name}.log");
    let log_path = run_dir.join(&log_name);

    // Open a log file for stdout/stderr. Fall back to Stdio::null() on failure.
    let (stdout_cfg, stderr_cfg) = match File::create(&log_path) {
        Ok(out_file) => {
            let err_file = out_file
                .try_clone()
                .context("failed to clone log file handle for stderr")?;
            (Stdio::from(out_file), Stdio::from(err_file))
        }
        Err(e) => {
            warn!(
                path = %log_path.display(),
                "failed to create copilot log file, output will be discarded: {}", e
            );
            (Stdio::null(), Stdio::null())
        }
    };

    info!(
        cmd = copilot_cmd,
        prompt = %prompt_file.display(),
        dir = %work_dir.display(),
        log = %log_path.display(),
        "spawning copilot (non-interactive, yolo)"
    );

    let mut cmd = tokio::process::Command::new(copilot_cmd);
    cmd.arg("-p")
        .arg(&content)
        .arg("--model")
        .arg(model)
        .arg("--yolo")
        .arg("--no-ask-user")
        .current_dir(work_dir)
        .stdout(stdout_cfg)
        .stderr(stderr_cfg)
        .stdin(Stdio::null());

    // Detach the child process from our controlling terminal by starting it
    // in a new session (setsid).  Without this, CLI tools that open /dev/tty
    // directly (e.g. for progress spinners or streaming output) will scribble
    // over the ratatui alternate-screen buffer and corrupt the TUI.
    //
    // SAFETY: setsid() is async-signal-safe and does not touch memory shared
    // with the parent.  It simply creates a new session for the child, which
    // removes its controlling terminal.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let result = cmd.spawn();

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
        error!(code, log = %log_path.display(), "copilot exited with non-zero status");

        // Include the tail of the log file in the error for debugging.
        let tail = read_tail(&log_path, 20);
        bail!(
            "copilot exited with code {} — see {}\n\n--- last lines ---\n{}",
            code,
            log_path.display(),
            tail
        )
    }
}

/// Read the last `n` lines from a file, returning them as a single string.
/// Returns an empty string on any I/O error.
fn read_tail(path: &Path, n: usize) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(len) = file.seek(SeekFrom::End(0)) else {
        return String::new();
    };
    // Read at most the last 8 KiB.
    let read_from = len.saturating_sub(8192);
    let _ = file.seek(SeekFrom::Start(read_from));
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return String::new();
    }
    let lines: Vec<&str> = buf.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
