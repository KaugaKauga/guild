# Guild

An autonomous software factory. Point it at a GitHub repo, label an issue, and
Guild will implement the change, open a draft PR, and keep fixing it until CI is
green — with no human in the loop.

## What it does

Guild is a Rust daemon that continuously monitors a GitHub repository for issues
carrying a specific label (default: `guild`). When it finds one, it drives the
issue through a fully automated pipeline that ends with a reviewed, green pull
request.

The daemon itself is **thin glue**. It handles orchestration, state, and
GitHub interaction. All the actual thinking — what to implement, how to fix a
test, how to respond to a review — is delegated to **GitHub Copilot CLI**
running in non-interactive mode with full permissions (`--yolo`).

## The pipeline

Every issue goes through these stages, in order:

```
INGEST ─▶ UNDERSTAND ─▶ PLAN ─▶ IMPLEMENT ─▶ VERIFY ─▶ SUBMIT ─▶ WATCH ◀──▶ FIX
                                                                      │
                                                                      ▼
                                                                    DONE
```

### Stage breakdown

| Stage | Who runs it | What happens |
|-------|-------------|--------------|
| **INGEST** | Daemon | Fetches the issue body, metadata, comments, and linked issues via `gh`. Saves everything as JSON + markdown into the run directory. |
| **UNDERSTAND** | Daemon | Shallow-clones the repo. Scans for CI workflows, contributing docs, dependency manifests. Reads `.guild/learnings.md` for repo-specific agent knowledge. Builds a directory tree. Creates a working branch (`guild/issue-{N}`). |
| **PLAN** | Copilot | Reads the issue + repo summary. Produces `plan.md` — which files to touch, what tests to write, what UI wiring is needed. |
| **IMPLEMENT** | Copilot | Writes production code and tests following the plan. Wires changes into the UI so they're actually reachable, not just isolated files. Appends any repo-specific learnings to `.guild/learnings.md`. |
| **VERIFY** | Copilot | Runs linting and basic checks. Fixes lint errors. Does **not** run full test suites that might hang (watch mode, browser tests). Trusts CI for that. |
| **SUBMIT** | Daemon | Commits all changes, pushes the branch, opens a **draft** pull request. Never marks it ready. |
| **WATCH** | Daemon | Polls the PR every cycle. Computes a "blocker fingerprint" from: failed CI checks, review decision, mergeable state, and `@guild` comment mentions. When the fingerprint changes, enters FIX. |
| **FIX** | Copilot | Reads the blocker report (failed checks, review comments, `@guild` mentions). Fixes the code. Appends any learnings to `.guild/learnings.md`. Daemon commits and pushes. Returns to WATCH. |
| **DONE** | Daemon | All checks green, review approved (or none), no `@guild` comments pending. Pipeline complete. |

### The WATCH ↔ FIX loop

After the PR is submitted, Guild enters a monitoring loop:

1. **WATCH** polls the PR status every 30 seconds (configurable)
2. It reacts to three types of signals:
   - **CI failures** — always triggers FIX
   - **Formal review with "changes requested"** — always triggers FIX, includes the reviewer's comments
   - **PR comments containing `@guild`** — triggers FIX (this is how you give it instructions)
3. Regular PR comments **without** `@guild` are ignored — not every conversation needs the agent
4. When all checks are green, review is approved (or none required), and there are no pending `@guild` mentions, the pipeline marks itself **DONE**

## How to use it

### Prerequisites

- [GitHub CLI](https://cli.github.com/) (`gh`) — authenticated (`gh auth login`)
- [GitHub Copilot CLI](https://docs.github.com/copilot/how-tos/copilot-cli) (`copilot`) — installed and authenticated
- Rust toolchain (`cargo`)

### Build

```
cd guild/daemon
cargo build --release
```

### Start the daemon

```
guild start owner/repo
```

Or with non-default options:

```
guild start owner/repo \
  --label guild \
  --poll-interval 30 \
  --model claude-opus-4.6 \
  --max-concurrent 5 \
  --no-tui
```

Alternatively, run directly with cargo:

```
cd daemon
cargo run -- start owner/repo
```

That's it. The daemon starts polling. To process an issue:

1. Create an issue on the target repo
2. Add the `guild` label to it
3. The daemon picks it up within 30 seconds and starts the pipeline

### Talk to it on a PR

Once Guild opens a draft PR, you can direct it with comments:

```
@guild the error handling in db.rs needs to use Result instead of unwrap
```

Any comment containing `@guild` will be picked up on the next poll cycle.
Comments without `@guild` are left alone.

### CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--repo` / `-r` | *required* | GitHub repo to watch (`owner/repo`) |
| `--label` / `-l` | `guild` | Only issues with this label get picked up |
| `--poll-interval` / `-p` | `30` | Seconds between polling cycles |
| `--copilot-cmd` | `copilot` | Path or name of the Copilot CLI binary |
| `--runs-dir` | `./runs` | Where run artifacts and worktrees are stored |

## Architecture

```
guild/
  daemon/                     # Rust binary — the orchestrator
    src/
      main.rs                 # CLI parsing, main poll loop
      db.rs                   # SQLite state persistence (guild.db)
      github.rs               # All gh/git CLI wrappers
      pipeline.rs             # Per-issue state machine (the 9 stages)
      copilot.rs              # Spawns copilot with: -p <prompt> --yolo --no-ask-user
    Cargo.toml
  agents/                     # Agent prompt templates (per-stage)
    plan.md
    implement.md
    verify.md
    fix.md
  runs/                       # Per-run artifacts (gitignored)
    guild.db                  # SQLite database (pipelines + completed ledger)
    {timestamp}-{repo}-{issue}/
      issue.json              # Raw issue metadata
      issue_body.md           # Issue description
      issue_comments.json     # Issue comments
      repo_summary.md         # Repo structure, CI, docs
      plan.md                 # Copilot's implementation plan
      verify_report.md        # Lint/check results
      blocker_report.md       # What's blocking the PR
      learnings.md            # Repo-specific learnings (copied from worktree)
      prompt_*.md             # Generated prompts for each stage
      worktree/               # Shallow clone of the repo (working branch)
```

### State persistence

Pipeline state lives in a SQLite database (`runs/guild.db`) with two tables:

- **`pipelines`** -- one row per active pipeline, updated after every stage transition
- **`completed`** -- permanent ledger of issues that reached Done (prevents re-runs)

SQLite WAL (write-ahead log) mode ensures crash safety: the database is always
in a consistent state, even after a hard kill. Stage transitions are persisted
immediately (not batched), so progress survives crashes mid-pipeline. When a
pipeline completes, it is atomically moved from `pipelines` to `completed` in a
single transaction.

On first run after upgrading, any existing `state.json` is automatically
migrated into the database and renamed to `state.json.bak`.

### Copilot integration

For each intelligent stage (PLAN, IMPLEMENT, VERIFY, FIX), the daemon:

1. Generates a prompt file with all relevant context (issue body, repo summary, blocker report, etc.)
2. Invokes Copilot in non-interactive mode with full permissions:
   ```
   copilot -p <prompt_content> --yolo --no-ask-user
   ```
3. Copilot reads the prompt, operates on files in the worktree, and exits
4. The daemon checks the exit code and advances (or retries)

The `--yolo` flag grants all permissions (file editing, shell commands, network access)
without approval prompts. The `--no-ask-user` flag prevents Copilot from pausing to
ask clarifying questions.

### Separation of concerns

The daemon makes **zero** implementation decisions. It only decides *when* to invoke
Copilot and *what context* to provide. All intelligence — what code to write, how to
fix a test, how to address a review comment — lives in the Copilot process.

### Repo learnings

Guild maintains a lightweight feedback loop via `.guild/learnings.md` in the target
repository. During the **UNDERSTAND** stage, the daemon reads this file (if it exists)
and injects its contents into every agent prompt (PLAN, IMPLEMENT, FIX). This gives
agents repo-specific context — build quirks, naming conventions, test patterns, common
gotchas — without requiring human curation.

At the end of **IMPLEMENT** and **FIX**, agents are instructed to reflect on anything
non-obvious they discovered and append it to `.guild/learnings.md`. The file is
committed alongside the rest of the code changes, so learnings accumulate over time
and are available to future Guild runs on the same repo.

## Design principles

1. **The agent reads everything.** Don't pre-parse or summarize. Hand it the raw issue, the raw repo tree, the raw CI output.
2. **Never mark a PR ready.** The daemon creates draft PRs only. A human decides when to merge.
3. **Never silence a failing check.** If Copilot can't fix it, the pipeline retries. If it's truly stuck, the daemon keeps polling until a human intervenes.
4. **State survives restarts.** Kill the daemon at any point, restart it, and it picks up where it left off.
5. **`@guild` is the control surface.** Comment on the PR to direct the agent. Everything else is ignored.
