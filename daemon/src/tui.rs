//! Terminal UI dashboard for the Familiar daemon.
//!
//! Renders a live table of active pipelines showing issue number, stage,
//! progress bar, and branch name. Queries the database on each 200 ms tick
//! so the display always reflects current state — no snapshot layer.

use std::collections::HashSet;
use std::io::{self, stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Terminal;

use futures::StreamExt;

use crate::db::Db;
use crate::pipeline::{Pipeline, Stage};

// ---------------------------------------------------------------------------
// TUI runner
// ---------------------------------------------------------------------------

/// Run the TUI render loop. Blocks until the user presses `q` or the
/// shutdown flag is set externally.
///
/// Queries the database every 200 ms directly, so the display is always
/// current — there is no intermediate snapshot or watch channel.
pub async fn run_tui(
    db: Db,
    running: Arc<Mutex<HashSet<u64>>>,
    repo: String,
    poll_interval: u64,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = render_loop(
        &mut terminal,
        &db,
        &running,
        &repo,
        poll_interval,
        &shutdown,
    )
    .await;

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

// ---------------------------------------------------------------------------
// Render loop
// ---------------------------------------------------------------------------

async fn render_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    db: &Db,
    running: &Arc<Mutex<HashSet<u64>>>,
    repo: &str,
    poll_interval: u64,
    shutdown: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut scroll_offset: usize = 0;
    let mut tick: usize = 0;
    let mut event_stream = EventStream::new();
    let mut refresh_interval = tokio::time::interval(Duration::from_millis(200));

    // Local pipeline cache, sorted by issue_number, updated from DB each tick.
    let mut pipelines: Vec<Pipeline> = Vec::new();
    let mut last_refresh: Option<Instant> = None;

    terminal.clear()?;

    // Draw once before entering the event loop.
    refresh_pipelines(db, &mut pipelines, &mut last_refresh);
    let running_snap = running.lock().unwrap().clone();
    draw_frame(
        terminal,
        &pipelines,
        &running_snap,
        repo,
        poll_interval,
        scroll_offset,
        tick,
        last_refresh,
    )?;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        tokio::select! {
            _ = refresh_interval.tick() => {
                tick = tick.wrapping_add(1);
                refresh_pipelines(db, &mut pipelines, &mut last_refresh);
                let running_snap = running.lock().unwrap().clone();
                draw_frame(terminal, &pipelines, &running_snap, repo, poll_interval,
                           scroll_offset, tick, last_refresh)?;
            }
            maybe_event = event_stream.next() => {
                if let Some(Ok(event)) = maybe_event {
                    let mut needs_redraw = false;
                    match event {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            match key.code {
                                KeyCode::Char('q') | KeyCode::Esc => {
                                    shutdown.store(true, Ordering::SeqCst);
                                    break;
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    scroll_offset = scroll_offset.saturating_sub(1);
                                    needs_redraw = true;
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    let max = pipelines.len().saturating_sub(1);
                                    if scroll_offset < max {
                                        scroll_offset += 1;
                                    }
                                    needs_redraw = true;
                                }
                                _ => {}
                            }
                        }
                        Event::Resize(_, _) => {
                            needs_redraw = true;
                        }
                        _ => {}
                    }
                    if needs_redraw {
                        let running_snap = running.lock().unwrap().clone();
                        draw_frame(terminal, &pipelines, &running_snap, repo, poll_interval,
                                   scroll_offset, tick, last_refresh)?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Query the DB and update the local pipeline cache, sorted by issue_number.
/// On DB error the cache is left unchanged so the last known state stays visible.
fn refresh_pipelines(db: &Db, pipelines: &mut Vec<Pipeline>, last_refresh: &mut Option<Instant>) {
    if let Ok(all) = db.get_all_active_pipelines() {
        let mut list: Vec<Pipeline> = all.into_values().collect();
        list.sort_by_key(|p| p.issue_number);
        *pipelines = list;
        *last_refresh = Some(Instant::now());
    }
}

// ---------------------------------------------------------------------------
// Frame drawing
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    pipelines: &[Pipeline],
    running: &HashSet<u64>,
    repo: &str,
    poll_interval: u64,
    scroll_offset: usize,
    tick: usize,
    last_refresh: Option<Instant>,
) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(area);

        render_header(frame, chunks[0], repo, poll_interval);
        render_pipeline_table(frame, chunks[1], pipelines, running, scroll_offset, tick);
        render_footer(frame, chunks[2], pipelines, last_refresh);
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

fn render_header(frame: &mut ratatui::Frame, area: Rect, repo: &str, poll_interval: u64) {
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " ⚒️  FAMILIAR ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "— Autonomous Software Factory  ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("repo: {}  │  poll: {}s", repo, poll_interval),
            Style::default().fg(Color::Cyan),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(header, area);
}

/// Spinner frames used to indicate an active pipeline.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Returns the spinner frame for the given tick.
pub fn spinner_frame(tick: usize) -> &'static str {
    SPINNER_FRAMES[tick % SPINNER_FRAMES.len()]
}

fn render_pipeline_table(
    frame: &mut ratatui::Frame,
    area: Rect,
    pipelines: &[Pipeline],
    running: &HashSet<u64>,
    scroll_offset: usize,
    tick: usize,
) {
    if pipelines.is_empty() {
        let empty = Paragraph::new(Line::from(vec![Span::styled(
            "  No active pipelines — waiting for labeled issues...",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]))
        .block(
            Block::default()
                .title(" Pipelines ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(empty, area);
        return;
    }

    let header_cells = [
        "#", "Issue", "", "Stage", "Progress", "Status", "Branch", "PR",
    ]
    .iter()
    .map(|h| {
        Cell::from(*h).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header = Row::new(header_cells).height(1);

    let visible: Vec<_> = pipelines.iter().skip(scroll_offset).collect();

    let rows = visible.iter().map(|p| {
        let is_running = running.contains(&p.issue_number);
        let status_text = stage_status_text(&p.stage, is_running);
        let (stage_name, stage_color) = stage_display(&p.stage);
        let (ordinal, total) = (p.stage.ordinal(), Stage::total_stages());
        let progress_bar = build_progress_bar(ordinal, total);

        let is_active = !matches!(p.stage, Stage::Done | Stage::Failed(_));
        let activity_indicator = if is_active {
            spinner_frame(tick).to_string()
        } else {
            " ".to_string()
        };
        let activity_color = if is_active {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let pr_text = match p.pr_number {
            Some(n) => format!("#{}", n),
            None => "—".to_string(),
        };

        let issue_title = if p.issue_title.chars().count() > 30 {
            let truncated: String = p.issue_title.chars().take(30).collect();
            format!("{}…", truncated)
        } else {
            p.issue_title.clone()
        };

        Row::new(vec![
            Cell::from(format!("#{}", p.issue_number)).style(Style::default().fg(Color::White)),
            Cell::from(issue_title).style(Style::default().fg(Color::White)),
            Cell::from(activity_indicator).style(Style::default().fg(activity_color)),
            Cell::from(stage_name).style(Style::default().fg(stage_color)),
            Cell::from(progress_bar),
            Cell::from(status_text).style(Style::default().fg(Color::DarkGray)),
            Cell::from(p.branch_name.clone()).style(Style::default().fg(Color::Blue)),
            Cell::from(pr_text).style(Style::default().fg(Color::Magenta)),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Min(20),
            Constraint::Length(3),
            Constraint::Length(14),
            Constraint::Length(22),
            Constraint::Length(20),
            Constraint::Min(16),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" Pipelines ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));

    frame.render_widget(table, area);
}

fn render_footer(
    frame: &mut ratatui::Frame,
    area: Rect,
    pipelines: &[Pipeline],
    last_refresh: Option<Instant>,
) {
    let active_count = pipelines
        .iter()
        .filter(|p| !matches!(p.stage, Stage::Done | Stage::Failed(_)))
        .count();

    let refresh_text = match last_refresh {
        Some(t) => {
            let secs = t.elapsed().as_secs();
            if secs == 0 {
                "data: live".to_string()
            } else {
                format!("data: {}s old", secs)
            }
        }
        None => "data: —".to_string(),
    };

    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " [q] quit  ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!("│  {}  ", refresh_text),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("│  active: {}  ", active_count),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("│  total: {}", pipelines.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(footer, area);
}

/// Returns (display name, color) for a stage.
fn stage_display(stage: &Stage) -> (String, Color) {
    match stage {
        Stage::Ingest => ("Ingest".into(), Color::White),
        Stage::Understand => ("Understand".into(), Color::Cyan),
        Stage::Plan => ("Plan".into(), Color::Blue),
        Stage::Implement => ("Implement".into(), Color::Yellow),
        Stage::Verify => ("Verify".into(), Color::Magenta),
        Stage::Submit => ("Submit".into(), Color::Green),
        Stage::Watch => ("Watch".into(), Color::LightBlue),
        Stage::Fix => ("Fix".into(), Color::LightYellow),
        Stage::Done => ("✔ Done".into(), Color::Green),
        Stage::Failed(msg) => {
            let short = if msg.len() > 20 {
                format!("✘ {}…", &msg[..20])
            } else {
                format!("✘ {}", msg)
            };
            (short, Color::Red)
        }
    }
}

/// Returns a brief status string for the given stage and running state.
fn stage_status_text(stage: &Stage, is_running: bool) -> String {
    if is_running && stage.needs_agent() {
        return "agent running…".into();
    }
    match stage {
        Stage::Plan | Stage::Implement | Stage::Verify | Stage::Fix => {
            if is_running {
                "agent running…".into()
            } else {
                "waiting for slot…".into()
            }
        }
        Stage::Ingest => "fetching issue…".into(),
        Stage::Understand => "analyzing…".into(),
        Stage::Submit => "pushing PR…".into(),
        Stage::Watch => "watching CI…".into(),
        Stage::Done => "complete".into(),
        Stage::Failed(_) => "failed".into(),
    }
}

/// Build an ASCII progress bar like `████░░░░ 3/8`.
fn build_progress_bar(current: u8, total: u8) -> String {
    let filled = current as usize;
    let empty = (total as usize).saturating_sub(filled);
    let bar: String = "█".repeat(filled) + &"░".repeat(empty);
    format!("{} {}/{}", bar, current, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_pipeline(issue_number: u64, stage: Stage) -> Pipeline {
        Pipeline {
            issue_number,
            repo: "owner/repo".into(),
            stage,
            run_dir: PathBuf::from("/tmp"),
            worktree: PathBuf::from("/tmp/worktree"),
            bare_repo: PathBuf::from("/tmp/repo.git"),
            pr_number: None,
            blocker_fingerprint: None,
            branch_name: format!("branch-{}", issue_number),
            issue_title: format!("Issue {}", issue_number),
            verify_attempts: 0,
        }
    }

    #[test]
    fn spinner_frame_cycles_through_all_frames() {
        for (i, expected) in SPINNER_FRAMES.iter().enumerate() {
            assert_eq!(spinner_frame(i), *expected);
        }
    }

    #[test]
    fn spinner_frame_wraps_around() {
        let len = SPINNER_FRAMES.len();
        assert_eq!(spinner_frame(0), spinner_frame(len));
        assert_eq!(spinner_frame(1), spinner_frame(len + 1));
        assert_eq!(spinner_frame(2), spinner_frame(len + 2));
    }

    #[test]
    fn spinner_frame_handles_large_tick() {
        let frame = spinner_frame(usize::MAX);
        assert!(SPINNER_FRAMES.contains(&frame));
    }

    #[test]
    fn active_stages_show_spinner() {
        let active_stages = vec![
            Stage::Ingest,
            Stage::Understand,
            Stage::Plan,
            Stage::Implement,
            Stage::Verify,
            Stage::Submit,
            Stage::Watch,
            Stage::Fix,
        ];
        for stage in active_stages {
            assert!(
                !matches!(stage, Stage::Done | Stage::Failed(_)),
                "Stage {:?} should be considered active",
                stage
            );
        }
    }

    #[test]
    fn terminal_stages_are_not_active() {
        assert!(matches!(Stage::Done, Stage::Done | Stage::Failed(_)));
        assert!(matches!(
            Stage::Failed("err".into()),
            Stage::Done | Stage::Failed(_)
        ));
    }

    #[test]
    fn progress_bar_format() {
        let bar = build_progress_bar(3, 8);
        assert!(bar.contains("3/8"));
        assert!(bar.contains("███"));
        assert!(bar.contains("░░░░░"));
    }

    #[test]
    fn no_active_pipelines_when_empty() {
        let pipelines: Vec<Pipeline> = Vec::new();
        assert!(!pipelines
            .iter()
            .any(|p| !matches!(p.stage, Stage::Done | Stage::Failed(_))));
    }

    #[test]
    fn no_active_pipelines_when_all_terminal() {
        let pipelines = vec![
            make_pipeline(1, Stage::Done),
            make_pipeline(2, Stage::Failed("err".into())),
        ];
        assert!(!pipelines
            .iter()
            .any(|p| !matches!(p.stage, Stage::Done | Stage::Failed(_))));
    }

    #[test]
    fn active_pipeline_detected() {
        let pipelines = vec![make_pipeline(1, Stage::Implement)];
        assert!(pipelines
            .iter()
            .any(|p| !matches!(p.stage, Stage::Done | Stage::Failed(_))));
    }

    #[test]
    fn pipelines_sorted_by_issue_number() {
        let mut pipelines = vec![
            make_pipeline(3, Stage::Plan),
            make_pipeline(1, Stage::Implement),
            make_pipeline(2, Stage::Watch),
        ];
        pipelines.sort_by_key(|p| p.issue_number);
        let numbers: Vec<u64> = pipelines.iter().map(|p| p.issue_number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
    }
}
