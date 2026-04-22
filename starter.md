You are helping me build my own "software factory" — an autonomous loop that
takes a Github Issue and drives it to a reviewed, green pull request with
minimal human intervention.

I want YOU (Copilot) to do the thinking. I will only write the thinnest
possible glue to pass data between stages. Any decision — what to implement,
how to react to a CI failure, how to respond to a review comment — is yours.

The factory runs in stages:

1. INGEST — pull the github issue via the gh cli. Read description,
   acceptance criteria, comments, linked tickets.

2. UNDERSTAND — read the target repo. Check .github/workflows/ for CI
   and PR comment triggers. Read a sample source + test file. Read
   CONTRIBUTING.md or AGENTS.md. Read .familiar/learnings.md if it exists —
   this file contains repo-specific learnings that MUST be followed in
   all subsequent stages.

3. PLAN — trace the full user path. What component does this replace or
   extend? How does a user reach it? What wiring is needed? Feature flag? When the plan is done add the plan 

4. IMPLEMENT — write the code. Write the tests. Wire it into the UI so
   a user can actually see it, not just an isolated file.

5. VERIFY — run lint. Do NOT run the full test suite locally if it might
   hang in watch/browser mode. Trust CI for test validation.

6. SUBMIT — create a DRAFT pull request. Never mark it ready yourself.

7. WATCH — poll the PR. Compute a "blocker fingerprint" from: comments
   (excl. bots), requested-changes, failed checks, mergeable state.
   When it changes, re-run the fix stage.

8. FIX — collect ALL relevant data (comments, CI logs, diff) and hand
   it to the agent. The agent decides the fix. Not the glue.

9. LEARN — append anything non-obvious to a per-repo learnings.md /.familiar config folder.

Repo setup (execute once per issue, before IMPLEMENT):

  BASE=~/repos                          # all clones live here
  REPO_DIR=$BASE/<owner>/<repo>
  WORK_DIR=$BASE/<owner>/<repo>-wt-<branch>

  1. Check: if $REPO_DIR is already a git repo
        → git -C $REPO_DIR fetch origin --prune
     Else
        → git clone https://github.com/<owner>/<repo> $REPO_DIR
  2. git -C $REPO_DIR worktree add $WORK_DIR -b <branch>
  3. All file edits happen inside $WORK_DIR.
  4. On exit (success OR error) → git -C $REPO_DIR worktree remove --force $WORK_DIR

Autonomy:
- You have full permission to clone, branch, edit files, commit, push, and open PRs.
- Do NOT ask the human for approval between stages. Run the full loop.
- Only pause if the issue is genuinely ambiguous or impossible — state the blocker
  once and propose an alternative, then wait for the human.
- If stuck after two self-correction attempts, exit and report what failed.

Principles:
- The agent reads everything. Don't pre-parse or summarise.
- Never mark a PR ready. Never silence a failing check.
- Keep commits atomic: one logical change per commit.
- If CI does not exist, create it as part of IMPLEMENT.

Startup:
- Ask the human for the repo (owner/repo) and issue number/title.
- Run INGEST then UNDERSTAND, show a summary, then proceed autonomously
  through PLAN → IMPLEMENT → VERIFY → SUBMIT without waiting for approval.
- We will build WATCH and FIX after the first PR lands.
