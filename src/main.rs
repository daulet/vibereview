mod claude;
mod models;

use std::io;

use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};

use claude::{list_projects, list_sessions, parse_session, ProjectInfo, SessionInfo};
use models::{Session, ToolInvocation, ToolType, Turn};

// =============================================================================
// App State
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    ProjectBrowser,
    SessionBrowser,
    SessionViewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Prompt,
    Thinking,
    ToolCalls,
    Diff,
}

impl DetailTab {
    fn next(self) -> Self {
        match self {
            DetailTab::Prompt => DetailTab::Thinking,
            DetailTab::Thinking => DetailTab::ToolCalls,
            DetailTab::ToolCalls => DetailTab::Diff,
            DetailTab::Diff => DetailTab::Prompt,
        }
    }

    fn prev(self) -> Self {
        match self {
            DetailTab::Prompt => DetailTab::Diff,
            DetailTab::Thinking => DetailTab::Prompt,
            DetailTab::ToolCalls => DetailTab::Thinking,
            DetailTab::Diff => DetailTab::ToolCalls,
        }
    }

    fn index(self) -> usize {
        match self {
            DetailTab::Prompt => 0,
            DetailTab::Thinking => 1,
            DetailTab::ToolCalls => 2,
            DetailTab::Diff => 3,
        }
    }
}

/// A view context for navigating turns (main session or subagent)
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub title: String,
    pub turns: Vec<Turn>,
    pub turn_list_state: ListState,
    pub active_tab: DetailTab,
    pub scroll_offset: u16,
    pub tool_scroll_offset: usize,
}

impl TurnContext {
    fn new(title: String, turns: Vec<Turn>) -> Self {
        let mut turn_list_state = ListState::default();
        if !turns.is_empty() {
            turn_list_state.select(Some(0));
        }
        Self {
            title,
            turns,
            turn_list_state,
            active_tab: DetailTab::Prompt,
            scroll_offset: 0,
            tool_scroll_offset: 0,
        }
    }

    fn selected_turn(&self) -> Option<&Turn> {
        self.turn_list_state.selected().and_then(|i| self.turns.get(i))
    }

    fn selected_tool(&self) -> Option<&ToolInvocation> {
        self.selected_turn()
            .and_then(|t| t.tool_invocations.get(self.tool_scroll_offset))
    }
}

pub struct App {
    pub view: View,
    pub projects: Vec<ProjectInfo>,
    pub sessions: Vec<SessionInfo>,
    pub session: Option<Session>,
    pub selected_project: Option<ProjectInfo>,
    pub project_list_state: ListState,
    pub session_list_state: ListState,
    /// Stack of turn contexts (main session at bottom, subagents pushed on top)
    pub context_stack: Vec<TurnContext>,
    pub should_quit: bool,
    pub error_message: Option<String>,
}

impl App {
    pub fn new() -> Self {
        let projects = list_projects();
        let mut project_list_state = ListState::default();
        if !projects.is_empty() {
            project_list_state.select(Some(0));
        }

        Self {
            view: View::ProjectBrowser,
            projects,
            sessions: Vec::new(),
            session: None,
            selected_project: None,
            project_list_state,
            session_list_state: ListState::default(),
            context_stack: Vec::new(),
            should_quit: false,
            error_message: None,
        }
    }

    /// Get the current (top) context
    pub fn current_context(&self) -> Option<&TurnContext> {
        self.context_stack.last()
    }

    /// Get mutable reference to current context
    pub fn current_context_mut(&mut self) -> Option<&mut TurnContext> {
        self.context_stack.last_mut()
    }

    /// Check if we're in a subagent view (depth > 1)
    pub fn is_subagent_view(&self) -> bool {
        self.context_stack.len() > 1
    }

    /// Get the breadcrumb path
    pub fn breadcrumb(&self) -> String {
        self.context_stack
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>()
            .join(" > ")
    }

    fn select_next_in_list(state: &mut ListState, len: usize) {
        if len == 0 {
            return;
        }
        let i = match state.selected() {
            Some(i) => if i >= len - 1 { 0 } else { i + 1 },
            None => 0,
        };
        state.select(Some(i));
    }

    fn select_prev_in_list(state: &mut ListState, len: usize) {
        if len == 0 {
            return;
        }
        let i = match state.selected() {
            Some(i) => if i == 0 { len - 1 } else { i - 1 },
            None => 0,
        };
        state.select(Some(i));
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        self.error_message = None;

        match self.view {
            View::ProjectBrowser => self.handle_project_browser_key(key),
            View::SessionBrowser => self.handle_session_browser_key(key),
            View::SessionViewer => self.handle_session_viewer_key(key),
        }
    }

    fn handle_project_browser_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Up => Self::select_prev_in_list(&mut self.project_list_state, self.projects.len()),
            KeyCode::Down => Self::select_next_in_list(&mut self.project_list_state, self.projects.len()),
            KeyCode::Enter => {
                if let Some(i) = self.project_list_state.selected() {
                    if let Some(project) = self.projects.get(i) {
                        self.selected_project = Some(project.clone());
                        self.sessions = list_sessions(&project.path);
                        self.session_list_state = ListState::default();
                        if !self.sessions.is_empty() {
                            self.session_list_state.select(Some(0));
                        }
                        self.view = View::SessionBrowser;
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_session_browser_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                self.view = View::ProjectBrowser;
            }
            KeyCode::Up => Self::select_prev_in_list(&mut self.session_list_state, self.sessions.len()),
            KeyCode::Down => Self::select_next_in_list(&mut self.session_list_state, self.sessions.len()),
            KeyCode::Enter => {
                if let Some(i) = self.session_list_state.selected() {
                    if let Some(session_info) = self.sessions.get(i) {
                        match parse_session(&session_info.path) {
                            Ok(session) => {
                                let context = TurnContext::new(
                                    session.name.clone(),
                                    session.turns.clone(),
                                );
                                self.session = Some(session);
                                self.context_stack = vec![context];
                                self.view = View::SessionViewer;
                            }
                            Err(e) => {
                                self.error_message = Some(format!("Failed to parse session: {}", e));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_session_viewer_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                // Pop subagent context or go back to session browser
                if self.context_stack.len() > 1 {
                    self.context_stack.pop();
                } else {
                    self.view = View::SessionBrowser;
                    self.session = None;
                    self.context_stack.clear();
                }
            }
            KeyCode::Up => {
                if let Some(ctx) = self.current_context_mut() {
                    let len = ctx.turns.len();
                    Self::select_prev_in_list(&mut ctx.turn_list_state, len);
                    ctx.scroll_offset = 0;
                    ctx.tool_scroll_offset = 0;
                }
            }
            KeyCode::Down => {
                if let Some(ctx) = self.current_context_mut() {
                    let len = ctx.turns.len();
                    Self::select_next_in_list(&mut ctx.turn_list_state, len);
                    ctx.scroll_offset = 0;
                    ctx.tool_scroll_offset = 0;
                }
            }
            KeyCode::Left => {
                if let Some(ctx) = self.current_context_mut() {
                    ctx.active_tab = ctx.active_tab.prev();
                    ctx.scroll_offset = 0;
                }
            }
            KeyCode::Right | KeyCode::Tab => {
                if let Some(ctx) = self.current_context_mut() {
                    ctx.active_tab = ctx.active_tab.next();
                    ctx.scroll_offset = 0;
                }
            }
            KeyCode::Char('j') => {
                if let Some(ctx) = self.current_context_mut() {
                    if ctx.active_tab == DetailTab::ToolCalls {
                        let tool_count = ctx.selected_turn()
                            .map(|t| t.tool_invocations.len())
                            .unwrap_or(0);
                        if ctx.tool_scroll_offset < tool_count.saturating_sub(1) {
                            ctx.tool_scroll_offset += 1;
                        }
                    } else {
                        ctx.scroll_offset = ctx.scroll_offset.saturating_add(3);
                    }
                }
            }
            KeyCode::Char('k') => {
                if let Some(ctx) = self.current_context_mut() {
                    if ctx.active_tab == DetailTab::ToolCalls {
                        ctx.tool_scroll_offset = ctx.tool_scroll_offset.saturating_sub(1);
                    } else {
                        ctx.scroll_offset = ctx.scroll_offset.saturating_sub(3);
                    }
                }
            }
            KeyCode::Char('g') => {
                if let Some(ctx) = self.current_context_mut() {
                    ctx.scroll_offset = 0;
                    ctx.tool_scroll_offset = 0;
                }
            }
            KeyCode::Char('G') => {
                if let Some(ctx) = self.current_context_mut() {
                    ctx.scroll_offset = u16::MAX;
                }
            }
            KeyCode::Enter => {
                // Try to open subagent if on Tool Calls tab and tool is openable
                self.try_open_subagent();
            }
            _ => {}
        }
    }

    fn try_open_subagent(&mut self) {
        let subagent_data = self.current_context().and_then(|ctx| {
            if ctx.active_tab != DetailTab::ToolCalls {
                return None;
            }
            ctx.selected_tool().and_then(|tool| {
                if let ToolType::Task { subagent_turns, subagent_type, description, .. } = &tool.tool_type {
                    if !subagent_turns.is_empty() {
                        let title = subagent_type.as_deref().unwrap_or(description.as_str()).to_string();
                        return Some((title, subagent_turns.clone()));
                    }
                }
                None
            })
        });

        if let Some((title, turns)) = subagent_data {
            let context = TurnContext::new(title, turns);
            self.context_stack.push(context);
        }
    }
}

// =============================================================================
// UI Rendering
// =============================================================================

fn ui(frame: &mut Frame, app: &mut App) {
    match app.view {
        View::ProjectBrowser => render_project_browser(frame, app),
        View::SessionBrowser => render_session_browser(frame, app),
        View::SessionViewer => render_session_viewer(frame, app),
    }

    if let Some(error) = &app.error_message {
        let area = frame.area();
        let error_area = Rect {
            x: area.x + 2,
            y: area.y + area.height - 2,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        let error_msg = Paragraph::new(error.as_str())
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
        frame.render_widget(error_msg, error_area);
    }
}

fn render_project_browser(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let items: Vec<ListItem> = app
        .projects
        .iter()
        .map(|p| ListItem::new(p.decoded_path.clone()))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Select Project (Enter to open, q to quit) "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut app.project_list_state);
}

fn render_session_browser(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let title = if let Some(p) = &app.selected_project {
        format!(" Sessions in {} (Esc to go back) ", p.decoded_path)
    } else {
        " Sessions ".to_string()
    };

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let modified = s.modified
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| {
                    let secs = d.as_secs();
                    let hours_ago = (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|n| n.as_secs())
                        .unwrap_or(secs) - secs) / 3600;
                    if hours_ago < 24 {
                        format!("{}h ago", hours_ago)
                    } else {
                        format!("{}d ago", hours_ago / 24)
                    }
                })
                .unwrap_or_default();

            let display = format!("{} {}", &s.name[..20.min(s.name.len())], modified);
            ListItem::new(display)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut app.session_list_state);
}

fn render_session_viewer(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(frame.area());

    render_turn_list(frame, app, chunks[0]);
    render_detail_panel(frame, app, chunks[1]);
}

fn render_turn_list(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);

    let is_subagent = app.is_subagent_view();

    let Some(ctx) = app.current_context_mut() else {
        return;
    };

    let items: Vec<ListItem> = ctx
        .turns
        .iter()
        .enumerate()
        .map(|(i, turn)| {
            let prompt_preview: String = turn.user_prompt
                .chars()
                .take(40)
                .collect::<String>()
                .replace('\n', " ");

            let tool_count = turn.tool_invocations.len();
            let tool_info = if tool_count > 0 {
                format!(" [{}]", tool_count)
            } else {
                String::new()
            };

            ListItem::new(format!("{}: {}{}", i + 1, prompt_preview, tool_info))
        })
        .collect();

    let title = if is_subagent {
        format!(" {} ({} turns) - Esc to go back ", ctx.title, ctx.turns.len())
    } else {
        format!(" Turns ({}) - Esc to go back ", ctx.turns.len())
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut ctx.turn_list_state);
}

fn render_detail_panel(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if app.is_subagent_view() { 2 } else { 1 }),
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    // Breadcrumb (only show if in subagent view)
    if app.is_subagent_view() {
        let breadcrumb = Paragraph::new(format!(" {} ", app.breadcrumb()))
            .style(Style::default().fg(Color::Cyan));
        frame.render_widget(breadcrumb, chunks[0]);
    }

    let tab_area = if app.is_subagent_view() { chunks[1] } else {
        // Merge breadcrumb area into tab area when not in subagent
        Rect {
            y: chunks[0].y,
            height: chunks[0].height + chunks[1].height,
            ..chunks[1]
        }
    };
    let content_area = chunks[2];
    let help_area = chunks[3];

    let Some(ctx) = app.current_context() else {
        return;
    };

    // Tab bar
    let tab_titles = vec!["Prompt", "Thinking", "Tool Calls", "Diff"];
    let tabs = Tabs::new(tab_titles)
        .block(Block::default().borders(Borders::ALL).title(" Details "))
        .select(ctx.active_tab.index())
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, tab_area);

    // Clear and render content
    frame.render_widget(Clear, content_area);
    let content_block = Block::default().borders(Borders::ALL);

    if let Some(turn) = ctx.selected_turn() {
        let content: Text = match ctx.active_tab {
            DetailTab::Prompt => render_prompt_tab(turn),
            DetailTab::Thinking => render_thinking_tab(turn),
            DetailTab::ToolCalls => render_tool_calls_tab(turn, ctx.tool_scroll_offset),
            DetailTab::Diff => render_diff_tab(turn),
        };

        let paragraph = Paragraph::new(content)
            .block(content_block)
            .wrap(Wrap { trim: false })
            .scroll((ctx.scroll_offset, 0));
        frame.render_widget(paragraph, content_area);
    } else {
        let paragraph = Paragraph::new("Select a turn to view details")
            .block(content_block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, content_area);
    }

    // Help line
    let help_text = if app.is_subagent_view() {
        " ↑/↓: Navigate | ←/→: Tabs | j/k: Scroll | Enter: Open | Esc: Back | q: Quit "
    } else {
        " ↑/↓: Navigate | ←/→: Tabs | j/k: Scroll | Enter: Open subagent | Esc: Back | q: Quit "
    };
    let help = Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, help_area);
}

/// Truncate a string to max_chars, adding "…" if truncated
fn truncate_str(s: &str, max_chars: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > max_chars {
        format!("{}…", s.chars().take(max_chars.saturating_sub(1)).collect::<String>())
    } else {
        s.to_string()
    }
}

fn render_prompt_tab(turn: &Turn) -> Text<'static> {
    let mut lines = vec![
        Line::styled("User Prompt:".to_string(), Style::default().fg(Color::Cyan).bold()),
        Line::from(""),
    ];

    for line in turn.user_prompt.lines() {
        lines.push(Line::from(line.to_string()));
    }

    if !turn.response.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled("─".repeat(40), Style::default().fg(Color::DarkGray)));
        lines.push(Line::from(""));
        lines.push(Line::styled("Response:".to_string(), Style::default().fg(Color::Green).bold()));
        lines.push(Line::from(""));
        for line in turn.response.lines() {
            lines.push(Line::from(line.to_string()));
        }
    }

    Text::from(lines)
}

fn render_thinking_tab(turn: &Turn) -> Text<'static> {
    if let Some(thinking) = &turn.thinking {
        let mut lines = vec![
            Line::styled("Model Thinking:".to_string(), Style::default().fg(Color::Magenta).bold()),
            Line::from(""),
        ];
        for line in thinking.lines() {
            lines.push(Line::from(line.to_string()));
        }
        Text::from(lines)
    } else {
        Text::styled("No thinking available for this turn".to_string(), Style::default().fg(Color::DarkGray))
    }
}

fn render_tool_calls_tab(turn: &Turn, scroll_offset: usize) -> Text<'static> {
    if turn.tool_invocations.is_empty() {
        return Text::styled("No tool calls in this turn".to_string(), Style::default().fg(Color::DarkGray));
    }

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::styled(
        format!("Tool Calls ({} total) - j/k to navigate, Enter to open subagent", turn.tool_invocations.len()),
        Style::default().fg(Color::Cyan).bold(),
    ));
    lines.push(Line::from(""));

    for (i, tool) in turn.tool_invocations.iter().enumerate() {
        let is_selected = i == scroll_offset;
        let is_openable = matches!(&tool.tool_type, ToolType::Task { subagent_turns, .. } if !subagent_turns.is_empty());

        // Visual indicator for openable tools
        let marker = if is_selected {
            if is_openable { "▶ " } else { "● " }
        } else {
            "  "
        };

        let header_style = if is_selected {
            Style::default().fg(Color::Yellow).bold()
        } else if is_openable {
            Style::default().fg(Color::Magenta)
        } else {
            Style::default().fg(Color::White)
        };

        // Tool label with context snippet
        let (tool_label, tool_context) = match &tool.tool_type {
            ToolType::Task { subagent_type, subagent_turns, description, .. } => {
                let type_info = subagent_type.as_deref().unwrap_or("Task");
                let label = if !subagent_turns.is_empty() {
                    format!("{} ({} turns) ⏎", type_info, subagent_turns.len())
                } else {
                    type_info.to_string()
                };
                let context = truncate_str(description, 40);
                (label, context)
            }
            ToolType::FileRead { path, .. } => {
                let name = path.rsplit('/').next().unwrap_or(path);
                (tool.tool_type.name().to_string(), truncate_str(name, 50))
            }
            ToolType::FileWrite { path, .. } => {
                let name = path.rsplit('/').next().unwrap_or(path);
                (tool.tool_type.name().to_string(), truncate_str(name, 50))
            }
            ToolType::FileEdit { path, .. } => {
                let name = path.rsplit('/').next().unwrap_or(path);
                (tool.tool_type.name().to_string(), truncate_str(name, 50))
            }
            ToolType::Command { command, .. } => {
                let cmd = command.lines().next().unwrap_or(command);
                (tool.tool_type.name().to_string(), truncate_str(cmd, 50))
            }
            ToolType::Search { pattern, .. } => {
                (tool.tool_type.name().to_string(), truncate_str(pattern, 50))
            }
            ToolType::WebFetch { url, .. } => {
                (tool.tool_type.name().to_string(), truncate_str(url, 50))
            }
            ToolType::WebSearch { query, .. } => {
                (tool.tool_type.name().to_string(), truncate_str(query, 50))
            }
            ToolType::TodoUpdate { todos } => {
                let summary = format!("{} items", todos.len());
                (tool.tool_type.name().to_string(), summary)
            }
            ToolType::Other { name } => {
                (name.clone(), String::new())
            }
        };

        let context_style = Style::default().fg(Color::DarkGray);
        let context_span = if tool_context.is_empty() {
            Span::raw("")
        } else {
            Span::styled(format!(" {}", tool_context), context_style)
        };

        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("[{}] ", i + 1), Style::default().fg(Color::DarkGray)),
            Span::styled(tool_label, header_style),
            context_span,
        ]));

        // Show details for selected tool
        if is_selected {
            lines.push(Line::from(""));

            // Input
            lines.push(Line::styled("  Input:".to_string(), Style::default().fg(Color::Green)));
            for line in tool.input_display.lines() {
                lines.push(Line::from(format!("    {}", line)));
            }

            lines.push(Line::from(""));

            // Output
            lines.push(Line::styled("  Output:".to_string(), Style::default().fg(Color::Yellow)));
            for line in tool.output_display.lines().take(30) {
                lines.push(Line::from(format!("    {}", line)));
            }
            if tool.output_display.lines().count() > 30 {
                lines.push(Line::styled("    ... (truncated)".to_string(), Style::default().fg(Color::DarkGray)));
            }

            // Hint for openable tools
            if is_openable {
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    "  Press Enter to view subagent conversation".to_string(),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::ITALIC),
                ));
            }

            lines.push(Line::from(""));
        }
    }

    Text::from(lines)
}

fn render_diff_tab(turn: &Turn) -> Text<'static> {
    let mut lines: Vec<Line> = Vec::new();
    let mut has_diff = false;

    fn render_diff(lines: &mut Vec<Line>, tool: &ToolInvocation, prefix: &str) -> bool {
        if let Some(diff) = tool.tool_type.diff() {
            let path = match &tool.tool_type {
                ToolType::FileEdit { path, .. } => path.clone(),
                ToolType::FileWrite { path, .. } => path.clone(),
                _ => "unknown".to_string(),
            };

            let header = if prefix.is_empty() {
                format!("─── {} ───", path)
            } else {
                format!("─── {} {} ───", prefix, path)
            };

            lines.push(Line::styled(header, Style::default().fg(Color::Cyan).bold()));
            lines.push(Line::from(""));

            for line in diff.lines() {
                let line_owned = line.to_string();
                let styled_line = if line.starts_with('+') && !line.starts_with("+++") {
                    Line::styled(line_owned, Style::default().fg(Color::Green))
                } else if line.starts_with('-') && !line.starts_with("---") {
                    Line::styled(line_owned, Style::default().fg(Color::Red))
                } else if line.starts_with("@@") {
                    Line::styled(line_owned, Style::default().fg(Color::Cyan))
                } else if line.starts_with("---") || line.starts_with("+++") {
                    Line::styled(line_owned, Style::default().fg(Color::White).bold())
                } else {
                    Line::from(line_owned)
                };
                lines.push(styled_line);
            }

            lines.push(Line::from(""));
            return true;
        }
        false
    }

    for tool in &turn.tool_invocations {
        if render_diff(&mut lines, tool, "") {
            has_diff = true;
        }

        // Collect diffs from subagent turns
        if let ToolType::Task { subagent_turns, subagent_type, .. } = &tool.tool_type {
            if !subagent_turns.is_empty() {
                let prefix = format!("[{}]", subagent_type.as_deref().unwrap_or("subagent"));
                for subturn in subagent_turns {
                    for subtool in &subturn.tool_invocations {
                        if render_diff(&mut lines, subtool, &prefix) {
                            has_diff = true;
                        }
                    }
                }
            }
        }
    }

    if !has_diff {
        return Text::styled("No diffs available for this turn".to_string(), Style::default().fg(Color::DarkGray));
    }

    Text::from(lines)
}

// =============================================================================
// Main
// =============================================================================

fn main() -> Result<()> {
    color_eyre::install()?;

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = App::new();

    while !app.should_quit {
        terminal.draw(|frame| ui(frame, &mut app))?;

        if event::poll(std::time::Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }
    }

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}
