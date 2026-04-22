//! Agent CLI integration.
//!
//! Guild dispatches intelligent stages (plan, implement, verify, fix) to an
//! external CLI agent.  Two backends are currently supported:
//!
//! - [`Backend::Copilot`] — GitHub Copilot CLI (`copilot`)
//! - [`Backend::Claude`]  — Anthropic Claude CLI (`claude`)
//!
//! Each backend owns its own flag shape, prompt-delivery mechanism (argv vs.
//! stdin), and authorisation flag (`--yolo` vs. `--permission-mode
//! bypassPermissions`).  Everything else — log-file redirection, setsid, exit
//! handling, tailing on failure — is shared by the common runner below.

use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Stdio;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{error, info, warn};

mod claude;
mod copilot;

/// Which external CLI to drive.
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum Backend {
    Copilot,
    Claude,
}

impl Backend {
    /// Default binary name for this backend when `--agent-cmd` is not set.
    pub fn default_cmd(self) -> &'static str {
        match self {
            Backend::Copilot => "copilot",
            Backend::Claude => "claude",
        }
    }

    /// Default model identifier if the user did not pass `--model`.
    pub fn default_model(self) -> &'static str {
        match self {
            Backend::Copilot => "claude-opus-4.6",
            Backend::Claude => "claude-opus-4-7",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Copilot => f.write_str("copilot"),
            Backend::Claude => f.write_str("claude"),
        }
    }
}

impl FromStr for Backend {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "copilot" => Ok(Backend::Copilot),
            "claude" => Ok(Backend::Claude),
            other => Err(format!(
                "unknown backend '{other}': expected 'copilot' or 'claude'"
            )),
        }
    }
}

/// How a backend delivers the prompt to the child process.
pub(crate) enum PromptDelivery {
    /// Prompt is baked into argv; nothing extra to do after spawn.
    Argv,
    /// Feed the prompt to child stdin, then close it.
    Stdin(String),
}

/// Contract each backend implements: build a `Command` (with args, model,
/// auth flag, etc.) and declare how the prompt gets delivered.
pub(crate) trait BackendInvocation {
    fn build(cmd: &str, model: &str, prompt: &str, work_dir: &Path) -> (Command, PromptDelivery);
}

/// Run the configured agent with the given prompt file in the given working
/// directory.
///
/// Reads the prompt file, dispatches to the backend-specific command builder,
/// then runs the common spawn/wait/log machinery.  Output is redirected to
/// `run_dir/agent_<stage>.log` so that interactive CLI spinners can't corrupt
/// the ratatui TUI.
pub async fn run(
    backend: Backend,
    cmd: &str,
    model: &str,
    prompt_file: &Path,
    work_dir: &Path,
    run_dir: &Path,
) -> Result<()> {
    let content = tokio::fs::read_to_string(prompt_file)
        .await
        .with_context(|| format!("failed to read prompt file: {}", prompt_file.display()))?;

    let stage_name = prompt_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("agent")
        .strip_prefix("prompt_")
        .unwrap_or("unknown");
    let log_name = format!("agent_{stage_name}.log");
    let log_path = run_dir.join(&log_name);

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
                "failed to create agent log file, output will be discarded: {}", e
            );
            (Stdio::null(), Stdio::null())
        }
    };

    let (mut command, delivery) = match backend {
        Backend::Copilot => copilot::CopilotBackend::build(cmd, model, &content, work_dir),
        Backend::Claude => claude::ClaudeBackend::build(cmd, model, &content, work_dir),
    };

    // Wire up shared process options: stdout/stderr -> log, stdin per delivery,
    // detached session so /dev/tty writes can't corrupt the TUI.
    command.stdout(stdout_cfg).stderr(stderr_cfg);
    match delivery {
        PromptDelivery::Argv => {
            command.stdin(Stdio::null());
        }
        PromptDelivery::Stdin(_) => {
            command.stdin(Stdio::piped());
        }
    }

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
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    info!(
        backend = %backend,
        cmd,
        prompt = %prompt_file.display(),
        dir = %work_dir.display(),
        log = %log_path.display(),
        "spawning agent (non-interactive, yolo)"
    );

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            error!(cmd, "failed to spawn agent process: {}", e);
            bail!(
                "failed to spawn '{}': {}. Check your --agent-cmd flag.",
                cmd,
                e
            );
        }
    };

    if let PromptDelivery::Stdin(payload) = delivery {
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(payload.as_bytes()).await {
                error!("failed to write prompt to agent stdin: {}", e);
            }
            // Drop stdin so the child sees EOF.
            drop(stdin);
        } else {
            warn!("child stdin unavailable; backend expected stdin delivery");
        }
    }

    let status = child
        .wait()
        .await
        .context("failed to wait on agent process")?;

    if status.success() {
        info!(backend = %backend, "agent completed successfully");
        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);
        error!(code, log = %log_path.display(), "agent exited with non-zero status");

        let tail = read_tail(&log_path, 20);
        bail!(
            "agent ({backend}) exited with code {} — see {}\n\n--- last lines ---\n{}",
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
