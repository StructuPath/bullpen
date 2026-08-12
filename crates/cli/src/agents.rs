//! `bullpen agents` — one screen for every background session.
//!
//! Daemonless by design: the dashboard is a read view over the SQLite store
//! plus process-liveness checks. Dispatching durably enqueues a prompt and
//! kicks an internal detached worker (see [`crate::bg`]); nothing here talks to it,
//! so quitting the dashboard never stops them. Attach-to-a-live-process
//! (interactive takeover) is Stage 2 and needs a per-session control socket;
//! for now a completed session is continued with `bullpen run -r <id>`.

use std::time::{Duration, Instant};

use anyhow::Result;
use bullpen_store::status::AgentStatus;
use bullpen_store::{Session, Store};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

/// A session paired with its derived dashboard state.
struct Row {
    session: Session,
    status: AgentStatus,
}

/// Group and order sessions for display: Working first, then Idle, then
/// Failed, then Completed; newest-updated first within each group. Pure so it
/// can be tested without a terminal.
fn arrange(rows: Vec<Row>) -> Vec<Row> {
    let mut rows = rows;
    rows.sort_by(|a, b| {
        a.status
            .group_rank()
            .cmp(&b.status.group_rank())
            .then_with(|| b.session.updated_at.cmp(&a.session.updated_at))
    });
    rows
}

fn load_rows(store: &Store) -> Result<Vec<Row>> {
    let rows = store
        .list_sessions()?
        .into_iter()
        .map(|session| {
            let alive = session.pid.is_some_and(crate::bg::pid_alive);
            let status = bullpen_store::status::for_session(&session, alive);
            Row { session, status }
        })
        .collect();
    Ok(arrange(rows))
}

/// Extra flags a dispatched session should carry. Kept minimal for Stage 1.
struct App {
    rows: Vec<Row>,
    selected: usize,
    input: String,
    peek: bool,
    error: Option<String>,
}

impl App {
    fn selected_session(&self) -> Option<&Session> {
        self.rows.get(self.selected).map(|r| &r.session)
    }

    fn refresh(&mut self, store: &Store) {
        match load_rows(store) {
            Ok(rows) => {
                self.rows = rows;
                if self.selected >= self.rows.len() {
                    self.selected = self.rows.len().saturating_sub(1);
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
}

pub fn run() -> Result<()> {
    let mut store = Store::open(&Store::default_path())?;

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut store);
    ratatui::restore();
    result
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    store: &mut Store,
) -> Result<()> {
    let mut app = App {
        rows: Vec::new(),
        selected: 0,
        input: String::new(),
        peek: false,
        error: None,
    };
    app.refresh(store);

    let poll_every = Duration::from_millis(250);
    let mut last_refresh = Instant::now();
    let refresh_every = Duration::from_secs(1);

    loop {
        terminal.draw(|f| draw(f, &app))?;

        if event::poll(poll_every)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break;
                    }
                    KeyCode::Esc => {
                        if app.peek {
                            app.peek = false;
                        } else if !app.input.is_empty() {
                            app.input.clear();
                        } else {
                            break;
                        }
                    }
                    KeyCode::Up => {
                        app.peek = false;
                        app.selected = app.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        app.peek = false;
                        if app.selected + 1 < app.rows.len() {
                            app.selected += 1;
                        }
                    }
                    KeyCode::Char(' ') if app.input.is_empty() => {
                        app.peek = !app.peek && app.selected_session().is_some();
                    }
                    KeyCode::Enter => {
                        let text = app.input.trim().to_string();
                        if text.len() >= 4 {
                            dispatch(&mut app, store, &text);
                            app.input.clear();
                        } else if !text.is_empty() {
                            app.error = Some("prompt too short — describe the task".into());
                        }
                    }
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => app.input.push(c),
                    _ => {}
                }
            }
        }

        if last_refresh.elapsed() >= refresh_every {
            app.refresh(store);
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn dispatch(app: &mut App, store: &mut Store, prompt: &str) {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());
    // Default provider for dashboard-dispatched sessions.
    let session = match store.create_session(&cwd, "codex", &default_codex_model()) {
        Ok(s) => s,
        Err(e) => {
            app.error = Some(format!("dispatch failed: {e}"));
            return;
        }
    };
    match crate::bg::enqueue_and_spawn(store, &session.id, prompt, &serde_json::json!({})) {
        Ok(_) => app.refresh(store),
        Err(e) => app.error = Some(format!("dispatch failed: {e}")),
    }
}

fn default_codex_model() -> String {
    crate::codex_cli_configured_model()
        .unwrap_or_else(|| bullpen_llm::codex::DEFAULT_CODEX_MODEL.to_string())
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    draw_list(f, chunks[1], app);
    draw_input(f, chunks[2], app);

    if app.peek {
        draw_peek(f, app);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let working = app
        .rows
        .iter()
        .filter(|r| r.status == AgentStatus::Working)
        .count();
    let line = Line::from(vec![
        Span::styled(
            " bullpen agents ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("· {} sessions · {working} working", app.rows.len())),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    let mut last_group: Option<AgentStatus> = None;

    for (i, row) in app.rows.iter().enumerate() {
        if last_group != Some(row.status) {
            lines.push(Line::from(Span::styled(
                row.status.label().to_string(),
                Style::default()
                    .fg(status_color(row.status))
                    .add_modifier(Modifier::BOLD),
            )));
            last_group = Some(row.status);
        }
        let selected = i == app.selected;
        let marker = if selected { "› " } else { "  " };
        let s = &row.session;
        let title = if s.title.is_empty() {
            "(untitled)"
        } else {
            &s.title
        };
        let title: String = title.chars().take(48).collect();
        let child = if s.parent_session_id.is_some() {
            " ↳"
        } else {
            ""
        };
        let text = format!(
            "{marker}{}  {:<10}  {:>6}/{:<6}  {title}{child}",
            &s.id[..8],
            s.provider,
            s.usage.input_tokens,
            s.usage.output_tokens,
        );
        let style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    if app.rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no sessions yet — type a task below and press Enter",
            Style::default().fg(Color::DarkGray),
        )));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let title = match &app.error {
        Some(e) => format!(" dispatch · {e} "),
        None => " dispatch (Enter) · ↑↓ select · Space peek · Esc quit ".into(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let content = if app.input.is_empty() {
        Span::styled("describe a task…", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(app.input.as_str())
    };
    f.render_widget(Paragraph::new(Line::from(content)).block(block), area);
}

fn draw_peek(f: &mut Frame, app: &App) {
    let Some(session) = app.selected_session() else {
        return;
    };
    let area = centered(70, 60, f.area());
    f.render_widget(Clear, area);

    // Latest assistant text from the durable transcript.
    let latest =
        match Store::open(&Store::default_path()).and_then(|s| s.path_messages(&session.id)) {
            Ok(messages) => messages
                .iter()
                .rev()
                .find(|m| m.role == bullpen_llm::Role::Assistant)
                .map(|m| m.text())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "(no output yet)".into()),
            Err(e) => format!("(could not read transcript: {e})"),
        };
    let lines = peek_lines(session, &latest);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" peek · Esc to close ");
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The peek panel's contents. Pure — the transcript read happens in the
/// caller — so what the panel says about a session can be tested without a
/// terminal or a store.
fn peek_lines(session: &Session, latest: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("{}  ({})", &session.id[..8], session.provider),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    // An isolated session's output is not in the directory the dashboard was
    // started from, so the panel has to say where it is.
    if let Some(path) = &session.worktree_path {
        lines.push(Line::from(Span::styled(
            format!("worktree {path}"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    for line in latest.lines() {
        lines.push(Line::from(line.to_string()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("continue with:  bullpen run -r {} \"…\"", &session.id[..8]),
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

fn status_color(status: AgentStatus) -> Color {
    match status {
        AgentStatus::Working => Color::Cyan,
        AgentStatus::Idle => Color::Gray,
        AgentStatus::Completed => Color::Green,
        AgentStatus::Failed => Color::Red,
    }
}

fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vert[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use bullpen_llm::Usage;

    fn row(id: &str, status: AgentStatus, updated: &str) -> Row {
        Row {
            session: Session {
                id: id.to_string(),
                title: String::new(),
                cwd: "/tmp".into(),
                provider: "codex".into(),
                model: "m".into(),
                usage: Usage::default(),
                created_at: updated.into(),
                updated_at: updated.into(),
                parent_session_id: None,
                status: "idle".into(),
                pid: None,
                worktree_path: None,
                worktree_branch: None,
            },
            status,
        }
    }

    fn rendered(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn arrange_groups_working_first_then_newest() {
        let rows = vec![
            row("done-old", AgentStatus::Completed, "2026-08-07 10:00"),
            row("work-a", AgentStatus::Working, "2026-08-07 09:00"),
            row("done-new", AgentStatus::Completed, "2026-08-07 11:00"),
            row("fail", AgentStatus::Failed, "2026-08-07 09:30"),
            row("work-b", AgentStatus::Working, "2026-08-07 12:00"),
        ];
        let ordered: Vec<String> = arrange(rows).into_iter().map(|r| r.session.id).collect();
        assert_eq!(
            ordered,
            vec![
                // Working, newest first
                "work-b", "work-a", // then Failed
                "fail",   // then Completed, newest first
                "done-new", "done-old",
            ]
        );
    }

    #[test]
    fn peek_lines_shows_the_worktree_for_an_isolated_session() {
        let mut r = row("0123456789ab", AgentStatus::Working, "2026-08-07 10:00");
        r.session.worktree_path = Some("/h/.bullpen/worktrees/0123456789ab".into());
        let lines = rendered(&peek_lines(&r.session, "output"));
        assert_eq!(lines[1], "worktree /h/.bullpen/worktrees/0123456789ab");
    }

    #[test]
    fn peek_lines_says_nothing_about_worktrees_for_a_shared_cwd_session() {
        let r = row("0123456789ab", AgentStatus::Working, "2026-08-07 10:00");
        let lines = rendered(&peek_lines(&r.session, "output"));
        assert!(!lines.iter().any(|l| l.contains("worktree")), "{lines:?}");
    }
}
