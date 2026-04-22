//! GitHub Copilot CLI backend.
//!
//! Invokes `copilot -p <prompt> --model <m> --yolo --no-ask-user`.
//! The prompt is passed on the command line (argv).  `--yolo` grants all
//! permissions (edit, shell, network) without prompting; `--no-ask-user`
//! prevents interactive clarifying questions.

use std::path::Path;

use tokio::process::Command;

use super::{BackendInvocation, PromptDelivery};

pub(crate) struct CopilotBackend;

impl BackendInvocation for CopilotBackend {
    fn build(cmd: &str, model: &str, prompt: &str, work_dir: &Path) -> (Command, PromptDelivery) {
        let mut c = Command::new(cmd);
        c.arg("-p")
            .arg(prompt)
            .arg("--model")
            .arg(model)
            .arg("--yolo")
            .arg("--no-ask-user")
            .current_dir(work_dir);
        (c, PromptDelivery::Argv)
    }
}
