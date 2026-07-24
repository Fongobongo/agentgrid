//! Stage 11.5: full-screen TUI dashboard (`ag tui`).
//!
//! A read-only monitoring dashboard over the control plane: a task list
//! (sidebar) with a live lifecycle phase per task, the selected task's event
//! stream (main, scrollable, colored), a node-status sub-list, a header bar
//! (server/totals/phase) and a footer keybind hint bar. Mutation happens via
//! the regular CLI (`ag run`, `ag task cancel`); the TUI is monitoring only.

use std::{io, time::Duration};

use agentgrid_common::{NodeStatus, TaskStatus};
use anyhow::Context;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use reqwest::Client;

const POLL_TICK: Duration = Duration::from_secs(2);

pub type Term = Terminal<CrosstermBackend<io::Stdout>>;

// ---------- app state (pure) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Starting,
    Working,
    Blocked,
    Done,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Working => "working",
            Phase::Blocked => "blocked",
            Phase::Done => "done",
        }
    }
    fn color(self) -> Color {
        match self {
            Phase::Starting => Color::DarkGray,
            Phase::Working => Color::Cyan,
            Phase::Blocked => Color::Yellow,
            Phase::Done => Color::Green,
        }
    }
    fn from_event(ty: &str, e: &serde_json::Value) -> Self {
        match ty {
            "tool" | "tool_call" | "file_change" | "progress" | "stdout" | "stderr" => {
                Phase::Working
            }
            "result" | "error" => Phase::Done,
            "status" => {
                if let Some(t) = e
                    .get("payload")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                {
                    if t.contains("succeeded") || t.contains("failed") || t.contains("cancelled") {
                        return Phase::Done;
                    }
                }
                Phase::Working
            }
            _ => Phase::Starting,
        }
    }
}

#[derive(Debug, Clone)]
struct TaskRow {
    id: String,
    adapter: String,
    status: TaskStatus,
}

#[derive(Debug, Clone)]
struct NodeRow {
    id: String,
    name: String,
    online: bool,
    load: u32,
}

#[derive(Debug, Clone)]
struct EventRow {
    seq: u64,
    formatted: String,
    color: Color,
}

fn format_event(e: &serde_json::Value) -> EventRow {
    let ty = e.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    let payload = e.get("payload").cloned().unwrap_or(serde_json::Value::Null);
    let text = payload.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let (formatted, color) = match ty {
        "stdout" => (format!("stdout {text}"), Color::DarkGray),
        "stderr" => (format!("stderr {text}"), Color::Red),
        "tool" | "tool_call" => {
            let tool = payload.get("tool").and_then(|v| v.as_str()).unwrap_or("?");
            let input = payload.get("input").and_then(|v| v.as_str()).unwrap_or("");
            (format!("tool {tool} {input}"), Color::Cyan)
        }
        "file_change" => {
            let path = payload.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let op = payload
                .get("op")
                .and_then(|v| v.as_str())
                .unwrap_or("change");
            (format!("file {op} {path}"), Color::Cyan)
        }
        "result" => (format!("result {text}"), Color::Green),
        "error" => (format!("error {text}"), Color::Red),
        "status" => (format!("status {text}"), Color::Yellow),
        _ => (format!("{ty} {text}"), Color::Gray),
    };
    EventRow {
        seq: e.get("sequence").and_then(|v| v.as_u64()).unwrap_or(0),
        formatted,
        color,
    }
}

#[derive(Debug, Default)]
struct AppState {
    server: String,
    tasks: Vec<TaskRow>,
    nodes: Vec<NodeRow>,
    events: Vec<EventRow>,
    phase: Phase,
    sidebar_index: usize,
    main_scroll: usize,
    focus: Focus,
    modal: Modal,
    follow: bool,
    no_color: bool,
    last_error: Option<String>,
}

impl AppState {
    fn new(server: String, no_color: bool) -> Self {
        Self {
            server,
            no_color,
            follow: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    #[default]
    Sidebar,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Modal {
    #[default]
    None,
    Help,
    TaskDetail,
}

impl AppState {
    fn selected_task(&self) -> Option<&TaskRow> {
        self.tasks.get(self.sidebar_index)
    }
    fn move_sidebar(&mut self, delta: i32) {
        if self.tasks.is_empty() {
            self.sidebar_index = 0;
            return;
        }
        let len = self.tasks.len() as i32;
        let mut i = self.sidebar_index as i32 + delta;
        if i < 0 {
            i = 0;
        }
        if i >= len {
            i = len - 1;
        }
        self.sidebar_index = i as usize;
    }
    fn scroll_main(&mut self, delta: i32) {
        let max = self.events.len();
        let mut s = self.main_scroll as i32 + delta;
        if s < 0 {
            s = 0;
        }
        if (s as usize) > max {
            s = max as i32;
        }
        self.main_scroll = s as usize;
        if delta != 0 {
            self.follow = false;
        }
    }
    fn reset_scroll(&mut self) {
        self.main_scroll = 0;
    }
}

// ---------- entry ----------

pub async fn run_dashboard(client: Client, base: String, no_color: bool) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let guard = TerminalGuard;
    let result = dashboard_loop(&mut terminal, client, base, no_color).await;
    drop(guard);
    result
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        if let Ok(mut terminal) = Terminal::new(backend) {
            let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
            let _ = terminal.show_cursor();
        }
    }
}

async fn dashboard_loop(
    terminal: &mut Term,
    client: Client,
    base: String,
    no_color: bool,
) -> anyhow::Result<()> {
    let mut state = AppState::new(base.clone(), no_color);
    refresh_list(&mut state, &client, &base).await;
    if let Some(id) = state.selected_task().map(|t| t.id.clone()) {
        refresh_events(&mut state, &client, &base, &id).await;
    }

    let mut tick = tokio::time::interval(POLL_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|f| render(f, &mut state))?;

        tokio::select! {
            _ = tick.tick() => {
                refresh_list(&mut state, &client, &base).await;
                if let Some(id) = state.selected_task().map(|t| t.id.clone()) {
                    if state.follow {
                        refresh_events(&mut state, &client, &base, &id).await;
                    }
                }
            }
            ev = read_key() => {
                if !handle_key(&mut state, ev?) {
                    return Ok(());
                }
            }
        }
    }
}

// ---------- input ----------

async fn read_key() -> anyhow::Result<Option<Event>> {
    tokio::task::spawn_blocking(|| -> anyhow::Result<Option<Event>> {
        if !event::poll(Duration::from_secs(0))? {
            return Ok(None);
        }
        Ok(Some(event::read()?))
    })
    .await?
}

fn handle_key(state: &mut AppState, ev: Option<Event>) -> bool {
    // Modal open: Esc/q/close-keys just close the modal.
    if state.modal != Modal::None
        && matches!(
            ev,
            Some(Event::Key(KeyEvent {
                code: KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Char('i'),
                ..
            }))
        )
    {
        state.modal = Modal::None;
        return true;
    }
    let action = match ev {
        Some(Event::Key(k)) => map_key(k),
        _ => return true,
    };
    match action {
        Action::Quit => return false,
        Action::Nothing => {}
        Action::Up => match state.focus {
            Focus::Sidebar => state.move_sidebar(-1),
            Focus::Main => state.scroll_main(1),
        },
        Action::Down => match state.focus {
            Focus::Sidebar => state.move_sidebar(1),
            Focus::Main => state.scroll_main(-1),
        },
        Action::PageUp => state.scroll_main(10),
        Action::PageDown => state.scroll_main(-10),
        Action::ToggleFocus => {
            state.focus = match state.focus {
                Focus::Sidebar => Focus::Main,
                Focus::Main => Focus::Sidebar,
            };
            if state.focus == Focus::Main && state.follow {
                state.reset_scroll();
            }
        }
        Action::RefreshList => {}
        Action::RefreshTask => {}
        Action::ToggleFollow => {
            state.follow = !state.follow;
            if state.follow {
                state.reset_scroll();
            }
        }
        Action::ToggleHelp => {
            state.modal = match state.modal {
                Modal::Help => Modal::None,
                _ => Modal::Help,
            };
        }
        Action::ToggleTaskDetail => {
            state.modal = match state.modal {
                Modal::TaskDetail => Modal::None,
                _ => Modal::TaskDetail,
            };
        }
    }
    true
}

enum Action {
    Quit,
    Nothing,
    Up,
    Down,
    PageUp,
    PageDown,
    ToggleFocus,
    RefreshList,
    RefreshTask,
    ToggleFollow,
    ToggleHelp,
    ToggleTaskDetail,
}

fn map_key(ev: KeyEvent) -> Action {
    let KeyEvent {
        code, modifiers, ..
    } = ev;
    if modifiers.contains(KeyModifiers::CONTROL) {
        return match code {
            KeyCode::Char('c') => Action::Quit,
            KeyCode::Char('u') => Action::PageUp,
            KeyCode::Char('d') => Action::PageDown,
            _ => Action::Nothing,
        };
    }
    match code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::Tab => Action::ToggleFocus,
        KeyCode::Char('r') => Action::RefreshList,
        KeyCode::Enter => Action::RefreshTask,
        KeyCode::Char('f') => Action::ToggleFollow,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char('i') => Action::ToggleTaskDetail,
        _ => Action::Nothing,
    }
}

// ---------- render ----------

fn render(f: &mut Frame, state: &mut AppState) {
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);
    render_header(f, state, outer[0]);
    render_body(f, state, outer[1]);
    render_footer(f, state, outer[2]);
    match state.modal {
        Modal::Help => render_help(f, area),
        Modal::TaskDetail => render_task_detail(f, state, area),
        Modal::None => {}
    }
}

fn render_header(f: &mut Frame, state: &AppState, area: Rect) {
    let online = state.nodes.iter().filter(|n| n.online).count();
    let title = format!(
        " agentgrid  {}  tasks:{}  nodes:{}/{} ",
        state.server,
        state.tasks.len(),
        online,
        state.nodes.len(),
    );
    let phase = colored_span(
        state,
        format!("  phase: {}", state.phase.label()),
        state.phase.color(),
    );
    let line = Line::from(vec![
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        phase,
    ]);
    f.render_widget(Block::default().borders(Borders::ALL).title(line), area);
}

fn render_body(f: &mut Frame, state: &mut AppState, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);
    render_sidebar(f, state, cols[0]);
    render_main(f, state, cols[1]);
}

fn render_sidebar(f: &mut Frame, state: &mut AppState, area: Rect) {
    let title = if state.focus == Focus::Sidebar {
        " Tasks (active) "
    } else {
        " Tasks "
    };
    let items: Vec<ListItem> = state
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let marker = if i == state.sidebar_index {
                "▶ "
            } else {
                "  "
            };
            let phase = colored_span(
                state,
                format!(" [{}]", phase_of(t.status)),
                phase_of_color(t.status),
            );
            ListItem::new(Line::from(vec![
                Span::raw(marker.to_string()),
                Span::raw(t.id.clone()),
                Span::raw(format!(" ({}) ", t.adapter)),
                phase,
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(focus_border(state, Focus::Sidebar)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut ls = ListState::default();
    ls.select(Some(state.sidebar_index));
    f.render_stateful_widget(list, area, &mut ls);

    if area.height >= 10 {
        let nodes_area = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area)[1];
        let nodes: Vec<ListItem> = state
            .nodes
            .iter()
            .map(|n| {
                let dot = colored_span(
                    state,
                    "●".to_string(),
                    if n.online { Color::Green } else { Color::Red },
                );
                ListItem::new(Line::from(vec![
                    dot,
                    Span::raw(format!(" {} (#{}) load={}", n.name, n.id, n.load)),
                ]))
            })
            .collect();
        f.render_widget(
            List::new(nodes).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Nodes ")
                    .border_style(Style::default()),
            ),
            nodes_area,
        );
    }
}

fn phase_of(s: TaskStatus) -> &'static str {
    use TaskStatus::*;
    match s {
        Queued | Assigned => "queued",
        Running => "running",
        Validating => "validating",
        Succeeded => "done",
        Failed => "done",
        Cancelled => "done",
    }
}

fn phase_of_color(s: TaskStatus) -> Color {
    use TaskStatus::*;
    match s {
        Queued | Assigned => Color::DarkGray,
        Running => Color::Cyan,
        Validating => Color::Yellow,
        Succeeded => Color::Green,
        Failed => Color::Red,
        Cancelled => Color::Magenta,
    }
}

fn render_main(f: &mut Frame, state: &mut AppState, area: Rect) {
    let title = match state.selected_task() {
        Some(t) => format!(" Events — {} ({}) ", t.id, t.adapter),
        None => " Events (pick a task) ".to_string(),
    };
    let border = focus_border(state, Focus::Main);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border);
    let total = state.events.len();
    let height = area.height.saturating_sub(2) as usize;
    let take = total.min(height);
    let start = total.saturating_sub(take).saturating_sub(state.main_scroll);
    let visible: Vec<Line> = state
        .events
        .iter()
        .skip(start)
        .take(take)
        .map(|r| {
            if state.no_color {
                Line::from(vec![Span::raw(format!("[{}] {}", r.seq, r.formatted))])
            } else {
                Line::from(vec![Span::styled(
                    format!("[{}] {}", r.seq, r.formatted),
                    Style::default().fg(r.color),
                )])
            }
        })
        .collect();
    let mut p = Paragraph::new(visible)
        .block(block)
        .wrap(Wrap { trim: false });
    if state.last_error.is_some() {
        p = p.style(Style::default().fg(Color::Red));
    }
    f.render_widget(p, area);
}

fn render_footer(f: &mut Frame, state: &AppState, area: Rect) {
    let hints = " ↑↓/jk move · Tab focus · r refresh · Enter reload · f follow · ? help · i detail · q quit ";
    let follow = if state.follow {
        "follow:on"
    } else {
        "follow:off"
    };
    let line = Line::from(vec![
        Span::raw(hints),
        Span::raw("  "),
        colored_span(state, follow.to_string(), Color::Cyan),
    ]);
    f.render_widget(
        Paragraph::new(line)
            .block(Block::default().borders(Borders::ALL))
            .alignment(Alignment::Left),
        area,
    );
}

fn render_help(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            " ag tui — keybind help ",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::raw(" ↑/↓ or j/k    move selection (sidebar or events)"),
        Line::raw(" PgUp/PgDn      scroll the events pane"),
        Line::raw(" Tab           toggle focus Sidebar ↔ Main"),
        Line::raw(" r             refresh the task list now"),
        Line::raw(" Enter         reload the selected task's events"),
        Line::raw(" f             toggle follow-the-latest mode"),
        Line::raw(" i             toggle task detail overlay"),
        Line::raw(" ?             this help"),
        Line::raw(" q / Esc       quit (closes any open overlay first)"),
        Line::raw(""),
        Line::raw(" Tasks are read-only here — create/cancel via `ag run` / `ag task cancel`."),
    ];
    let area = centered_rect(area, 60, 70);
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Help "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_task_detail(f: &mut Frame, state: &AppState, area: Rect) {
    let body = match state.selected_task() {
        Some(t) => vec![
            Line::from(Span::styled(
                format!(" Task {} ", t.id),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::raw(format!("adapter : {}", t.adapter)),
            Line::raw(format!("status  : {:?}", t.status)),
            Line::raw(format!("server  : {}", state.server)),
            Line::raw(format!("events  : {}", state.events.len())),
        ],
        None => vec![Line::raw("no task selected")],
    };
    let area = centered_rect(area, 50, 50);
    f.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Task detail "),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn colored_span(state: &AppState, text: String, color: Color) -> Span<'static> {
    if state.no_color {
        Span::raw(text)
    } else {
        Span::styled(text, Style::default().fg(color))
    }
}

fn focus_border(state: &AppState, which: Focus) -> Style {
    if state.focus == which && !state.no_color {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn centered_rect(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let h = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(h[1])[1]
}

// ---------- HTTP refresh ----------

async fn refresh_list(state: &mut AppState, client: &Client, base: &str) {
    if let Err(e) = fetch_tasks(state, client, base).await {
        state.last_error = Some(format!("{e}"));
    }
    if let Err(e) = fetch_nodes(state, client, base).await {
        state.last_error = Some(format!("{e}"));
    }
}

async fn fetch_tasks(state: &mut AppState, client: &Client, base: &str) -> anyhow::Result<()> {
    let resp = client.get(format!("{base}/v1/tasks")).send().await?;
    let tasks: Vec<serde_json::Value> = resp.json().await.context("parse tasks")?;
    state.tasks = tasks
        .iter()
        .map(|t| TaskRow {
            id: t
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            adapter: t
                .get("adapter")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            status: serde_json::from_value(
                t.get("status")
                    .cloned()
                    .unwrap_or(serde_json::Value::String("queued".into())),
            )
            .unwrap_or(TaskStatus::Queued),
        })
        .collect();
    Ok(())
}

async fn fetch_nodes(state: &mut AppState, client: &Client, base: &str) -> anyhow::Result<()> {
    let resp = client.get(format!("{base}/v1/nodes")).send().await?;
    let nodes: Vec<serde_json::Value> = resp.json().await.context("parse nodes")?;
    state.nodes = nodes
        .iter()
        .map(|n| NodeRow {
            id: n
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            name: n
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string(),
            online: serde_json::from_value::<NodeStatus>(
                n.get("status")
                    .cloned()
                    .unwrap_or(serde_json::Value::String("offline".into())),
            )
            .map(|s| s == NodeStatus::Online)
            .unwrap_or(false),
            load: n
                .get("running_attempts")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
        })
        .collect();
    Ok(())
}

async fn refresh_events(state: &mut AppState, client: &Client, base: &str, task_id: &str) {
    match fetch_events(client, base, task_id).await {
        Ok(events) => {
            state.events = events.iter().map(format_event).collect();
            state.phase = events
                .last()
                .and_then(|e| {
                    e.get("type")
                        .and_then(|v| v.as_str())
                        .map(|ty| Phase::from_event(ty, e))
                })
                .unwrap_or(Phase::Starting);
            if state.phase != Phase::Done {
                match pending_approval_for_task(client, base, task_id).await {
                    Ok(true) => state.phase = Phase::Blocked,
                    Ok(false) => {}
                    Err(e) => state.last_error = Some(format!("{e}")),
                }
            }
            if state.follow {
                state.reset_scroll();
            }
            state.last_error = None;
        }
        Err(e) => state.last_error = Some(format!("{e}")),
    }
}

async fn fetch_events(
    client: &Client,
    base: &str,
    task_id: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let resp = client
        .get(format!("{base}/v1/tasks/{task_id}/events"))
        .send()
        .await?;
    resp.json().await.context("parse events")
}

async fn pending_approval_for_task(
    client: &Client,
    base: &str,
    task_id: &str,
) -> anyhow::Result<bool> {
    let resp = client
        .get(format!("{base}/v1/approvals"))
        .query(&[("status", "pending")])
        .send()
        .await?;
    let views: Vec<serde_json::Value> = resp.json().await?;
    Ok(views
        .iter()
        .any(|v| v.get("task_id").and_then(|t| t.as_str()) == Some(task_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn phase_from_event_table() {
        assert_eq!(Phase::from_event("tool_call", &json!({})), Phase::Working);
        assert_eq!(Phase::from_event("result", &json!({})), Phase::Done);
        assert_eq!(
            Phase::from_event("status", &json!({ "payload": { "text": "succeeded" } })),
            Phase::Done
        );
        assert_eq!(Phase::from_event("status", &json!({})), Phase::Working);
        assert_eq!(Phase::from_event("unknown", &json!({})), Phase::Starting);
    }

    #[test]
    fn format_event_tool_and_file() {
        let e = json!({ "sequence": 7i64, "type": "tool_call", "payload": { "tool": "bash", "input": "ls" } });
        let r = format_event(&e);
        assert_eq!(r.seq, 7);
        assert!(r.formatted.contains("tool bash ls"), "{}", r.formatted);
        assert_eq!(r.color, Color::Cyan);
        let e2 = json!({ "sequence": 3i64, "type": "file_change", "payload": { "path": "src/x.rs", "op": "edit" } });
        assert!(format_event(&e2).formatted.contains("file edit src/x.rs"));
    }

    #[test]
    fn sidebar_move_and_scroll_clamp() {
        let mut st = AppState {
            tasks: vec![
                TaskRow {
                    id: "a".into(),
                    adapter: "m".into(),
                    status: TaskStatus::Queued,
                },
                TaskRow {
                    id: "b".into(),
                    adapter: "m".into(),
                    status: TaskStatus::Queued,
                },
            ],
            follow: true,
            events: vec![EventRow {
                seq: 1,
                formatted: "x".into(),
                color: Color::Gray,
            }],
            ..Default::default()
        };
        st.move_sidebar(-5);
        assert_eq!(st.sidebar_index, 0);
        st.move_sidebar(99);
        assert_eq!(st.sidebar_index, 1);
        st.scroll_main(1);
        assert!(!st.follow, "manual scroll disables follow");
    }

    #[test]
    fn map_key_basics() {
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(map_key(j), Action::Down));
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(map_key(q), Action::Quit));
        let cc = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(map_key(cc), Action::Quit));
        let qm = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(matches!(map_key(qm), Action::ToggleHelp));
    }
}
