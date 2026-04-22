//! Anthropic Claude CLI backend.
//!
//! Invokes `claude -p --permission-mode bypassPermissions --model <m>
//! --output-format text`.  The prompt is piped via stdin so that arbitrarily
//! large prompts don't hit argv size limits (agent prompts include full
//! issue bodies, repo summaries, and blocker reports).
//!
//! `--permission-mode bypassPermissions` is Claude's analogue of Copilot's
//! `--yolo` — it disables all approval prompts for file edits, shell
//! commands, and network access.

use std::path::Path;

use tokio::process::Command;

use super::{BackendInvocation, PromptDelivery};

pub(crate) struct ClaudeBackend;

impl BackendInvocation for ClaudeBackend {
    fn build(cmd: &str, model: &str, prompt: &str, work_dir: &Path) -> (Command, PromptDelivery) {
        let mut c = Command::new(cmd);
        c.arg("-p")
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .arg("--model")
            .arg(model)
            .arg("--output-format")
            .arg("text")
            .current_dir(work_dir);
        (c, PromptDelivery::Stdin(prompt.to_string()))
    }
}
