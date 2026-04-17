//! Terminal UI dashboard for the Guild daemon.
//!
//! Renders a live table of active pipelines showing issue number, stage,
//! progress bar, and branch name.  Driven by a `tokio::sync::watch` channel
//! that receives `DaemonState` updates from the main daemon loop.

use std::io::{self, stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
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

use crate::pipeline::Stage;

// ---------------------------------------------------------------------------
// Shared state types
// ---------------------------------------------------------------------------

/// Snapshot of a single pipeline for TUI rendering.
#[derive(Clone, Debug)]
pub struct PipelineSnapshot {
    pub issue_number: u64,
    pub stage: Stage,
    pub branch_name: String,
    pub pr_number: Option<u64>,
}

/// Full daemon state sent to the TUI on each cycle.
#[derive(Clone, Debug)]
pub struct DaemonState {
    pub pipelines: Vec<PipelineSnapshot>,
    pub last_poll: Option<chrono::DateTime<chrono::Utc>>,
    pub cycle_count: u64,
    pub repo: String,
    pub poll_interval: u64,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            pipelines: Vec::new(),
            last_poll: None,
            cycle_count: 0,
            repo: String::new(),
            poll_interval: 30,
        }
    }
}

// ---------------------------------------------------------------------------
// TUI runner
// ---------------------------------------------------------------------------

/// Run the TUI render loop.  Blocks until the user presses `q` or the
/// shutdown flag is set externally.
pub async fn run_tui(
    state_rx: tokio::sync::watch::Receiver<DaemonState>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = render_loop(&mut terminal, state_rx, &shutdown).await;

    // Restore terminal
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

async fn render_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state_rx: tokio::sync::watch::Receiver<DaemonState>,
    shutdown: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut scroll_offset: usize = 0;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let state = state_rx.borrow().clone();

        terminal.draw(|frame| {
            let area = frame.area();

            // Layout: header (3) | table (stretch) | footer (3)
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(3),
                ])
                .split(area);

            render_header(frame, chunks[0], &state);
            render_pipeline_table(frame, chunks[1], &state, scroll_offset);
            render_footer(frame, chunks[2], &state);
        })?;

        // Poll for keyboard events (non-blocking with timeout)
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            shutdown.store(true, Ordering::SeqCst);
                            break;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            scroll_offset = scroll_offset.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let max = state_rx.borrow().pipelines.len().saturating_sub(1);
                            if scroll_offset < max {
                                scroll_offset += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

fn render_header(frame: &mut ratatui::Frame, area: Rect, state: &DaemonState) {
    let cycle_text = format!("cycle: {}", state.cycle_count);
    let poll_text = format!("poll: {}s", state.poll_interval);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " ⚒️  GUILD ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "— Autonomous Software Factory  ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("repo: {}  │  {}  │  {}", state.repo, poll_text, cycle_text),
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

fn render_pipeline_table(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &DaemonState,
    scroll_offset: usize,
) {
    if state.pipelines.is_empty() {
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

    let header_cells = ["#", "Stage", "Progress", "Branch", "PR"].iter().map(|h| {
        Cell::from(*h).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header = Row::new(header_cells).height(1);

    let visible_pipelines: Vec<_> = state.pipelines.iter().skip(scroll_offset).collect();

    let rows = visible_pipelines.iter().map(|p| {
        let (stage_name, stage_color) = stage_display(&p.stage);
        let (ordinal, total) = (p.stage.ordinal(), Stage::total_stages());
        let progress_bar = build_progress_bar(ordinal, total);

        let pr_text = match p.pr_number {
            Some(n) => format!("#{}", n),
            None => "—".to_string(),
        };

        Row::new(vec![
            Cell::from(format!("#{}", p.issue_number)).style(Style::default().fg(Color::White)),
            Cell::from(stage_name).style(Style::default().fg(stage_color)),
            Cell::from(progress_bar),
            Cell::from(p.branch_name.clone()).style(Style::default().fg(Color::Blue)),
            Cell::from(pr_text).style(Style::default().fg(Color::Magenta)),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(22),
            Constraint::Min(20),
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

fn render_footer(frame: &mut ratatui::Frame, area: Rect, state: &DaemonState) {
    let last_poll_text = match state.last_poll {
        Some(t) => {
            let elapsed = chrono::Utc::now().signed_duration_since(t);
            format!("last poll: {}s ago", elapsed.num_seconds())
        }
        None => "last poll: —".to_string(),
    };

    let active_count = state
        .pipelines
        .iter()
        .filter(|p| !matches!(p.stage, Stage::Done | Stage::Failed(_)))
        .count();

    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " [q] quit  ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!("│  {}  ", last_poll_text),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("│  active: {}  ", active_count),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("│  total: {}", state.pipelines.len()),
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

/// Build an ASCII progress bar like `████░░░░ 3/8`.
fn build_progress_bar(current: u8, total: u8) -> String {
    let filled = current as usize;
    let empty = (total as usize).saturating_sub(filled);
    let bar: String = "█".repeat(filled) + &"░".repeat(empty);
    format!("{} {}/{}", bar, current, total)
}
