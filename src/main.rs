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
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};

use claude::{list_projects, list_sessions, parse_session, ProjectInfo, SessionInfo};
use models::{Session, ToolType, Turn};

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

pub struct App {
    pub view: View,
    pub projects: Vec<ProjectInfo>,
    pub sessions: Vec<SessionInfo>,
    pub session: Option<Session>,
    pub selected_project: Option<ProjectInfo>,
    pub project_list_state: ListState,
    pub session_list_state: ListState,
    pub turn_list_state: ListState,
    pub active_tab: DetailTab,
    pub scroll_offset: u16,
    pub tool_scroll_offset: usize,
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
            turn_list_state: ListState::default(),
            active_tab: DetailTab::Prompt,
            scroll_offset: 0,
            tool_scroll_offset: 0,
            should_quit: false,
            error_message: None,
        }
    }

    pub fn selected_turn(&self) -> Option<&Turn> {
        self.session.as_ref().and_then(|s| {
            self.turn_list_state.selected().and_then(|i| s.turns.get(i))
        })
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
        // Clear error on any key
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
            KeyCode::Char('q') | KeyCode::Esc => {
                self.view = View::ProjectBrowser;
            }
            KeyCode::Up => Self::select_prev_in_list(&mut self.session_list_state, self.sessions.len()),
            KeyCode::Down => Self::select_next_in_list(&mut self.session_list_state, self.sessions.len()),
            KeyCode::Enter => {
                if let Some(i) = self.session_list_state.selected() {
                    if let Some(session_info) = self.sessions.get(i) {
                        match parse_session(&session_info.path) {
                            Ok(session) => {
                                self.session = Some(session);
                                self.turn_list_state = ListState::default();
                                if let Some(s) = &self.session {
                                    if !s.turns.is_empty() {
                                        self.turn_list_state.select(Some(0));
                                    }
                                }
                                self.active_tab = DetailTab::Prompt;
                                self.scroll_offset = 0;
                                self.tool_scroll_offset = 0;
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
            KeyCode::Char('q') | KeyCode::Esc => {
                self.view = View::SessionBrowser;
                self.session = None;
            }
            KeyCode::Up => {
                if let Some(s) = &self.session {
                    Self::select_prev_in_list(&mut self.turn_list_state, s.turns.len());
                    self.scroll_offset = 0;
                    self.tool_scroll_offset = 0;
                }
            }
            KeyCode::Down => {
                if let Some(s) = &self.session {
                    Self::select_next_in_list(&mut self.turn_list_state, s.turns.len());
                    self.scroll_offset = 0;
                    self.tool_scroll_offset = 0;
                }
            }
            KeyCode::Left => {
                self.active_tab = self.active_tab.prev();
                self.scroll_offset = 0;
                self.tool_scroll_offset = 0;
            }
            KeyCode::Right | KeyCode::Tab => {
                self.active_tab = self.active_tab.next();
                self.scroll_offset = 0;
                self.tool_scroll_offset = 0;
            }
            KeyCode::Char('j') => {
                if self.active_tab == DetailTab::ToolCalls {
                    if let Some(turn) = self.selected_turn() {
                        if self.tool_scroll_offset < turn.tool_invocations.len().saturating_sub(1) {
                            self.tool_scroll_offset += 1;
                        }
                    }
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_add(3);
                }
            }
            KeyCode::Char('k') => {
                if self.active_tab == DetailTab::ToolCalls {
                    self.tool_scroll_offset = self.tool_scroll_offset.saturating_sub(1);
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
            }
            KeyCode::Char('g') => {
                self.scroll_offset = 0;
                self.tool_scroll_offset = 0;
            }
            KeyCode::Char('G') => {
                self.scroll_offset = u16::MAX;
            }
            _ => {}
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

    // Render error message if any
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

            let display = if s.name.starts_with("agent-") {
                format!("{} (agent) {}", &s.name[..20.min(s.name.len())], modified)
            } else {
                format!("{} {}", &s.name[..20.min(s.name.len())], modified)
            };
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
    let Some(session) = &app.session else {
        return;
    };

    let items: Vec<ListItem> = session
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

    let title = format!(" Turns ({}) - Esc to go back ", session.turns.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut app.turn_list_state);
}

fn render_detail_panel(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // Tab bar
    let tab_titles = vec!["Prompt", "Thinking", "Tool Calls", "Diff"];
    let tabs = Tabs::new(tab_titles)
        .block(Block::default().borders(Borders::ALL).title(" Details "))
        .select(app.active_tab.index())
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, chunks[0]);

    // Content
    let content_block = Block::default().borders(Borders::ALL);

    if let Some(turn) = app.selected_turn() {
        let content: Text = match app.active_tab {
            DetailTab::Prompt => {
                let mut lines = vec![
                    Line::styled("User Prompt:", Style::default().fg(Color::Cyan).bold()),
                    Line::from(""),
                ];
                for line in turn.user_prompt.lines() {
                    lines.push(Line::from(line));
                }

                if !turn.response.is_empty() {
                    lines.push(Line::from(""));
                    lines.push(Line::styled("─".repeat(40), Style::default().fg(Color::DarkGray)));
                    lines.push(Line::from(""));
                    lines.push(Line::styled("Response:", Style::default().fg(Color::Green).bold()));
                    lines.push(Line::from(""));
                    for line in turn.response.lines() {
                        lines.push(Line::from(line));
                    }
                }

                Text::from(lines)
            }
            DetailTab::Thinking => {
                if let Some(thinking) = &turn.thinking {
                    let mut lines = vec![
                        Line::styled("Model Thinking:", Style::default().fg(Color::Magenta).bold()),
                        Line::from(""),
                    ];
                    for line in thinking.lines() {
                        lines.push(Line::from(line));
                    }
                    Text::from(lines)
                } else {
                    Text::styled("No thinking available for this turn", Style::default().fg(Color::DarkGray))
                }
            }
            DetailTab::ToolCalls => {
                if turn.tool_invocations.is_empty() {
                    Text::styled("No tool calls in this turn", Style::default().fg(Color::DarkGray))
                } else {
                    render_tool_calls(turn, app.tool_scroll_offset)
                }
            }
            DetailTab::Diff => {
                render_diffs(turn)
            }
        };

        let paragraph = Paragraph::new(content)
            .block(content_block)
            .wrap(Wrap { trim: false })
            .scroll((app.scroll_offset, 0));
        frame.render_widget(paragraph, chunks[1]);
    } else {
        let paragraph = Paragraph::new("Select a turn to view details")
            .block(content_block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, chunks[1]);
    }

    // Help line
    let help_text = " ↑/↓: Navigate | ←/→: Tabs | j/k: Scroll | g/G: Top/Bottom | Esc: Back ";
    let help = Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, chunks[2]);
}

fn render_tool_calls(turn: &Turn, scroll_offset: usize) -> Text<'static> {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::styled(
        format!("Tool Calls ({} total) - j/k to scroll between tools", turn.tool_invocations.len()),
        Style::default().fg(Color::Cyan).bold(),
    ));
    lines.push(Line::from(""));

    for (i, tool) in turn.tool_invocations.iter().enumerate() {
        let is_selected = i == scroll_offset;
        let marker = if is_selected { "▶ " } else { "  " };

        // Tool header
        let header_style = if is_selected {
            Style::default().fg(Color::Yellow).bold()
        } else {
            Style::default().fg(Color::White)
        };

        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("[{}] ", i + 1), Style::default().fg(Color::DarkGray)),
            Span::styled(tool.tool_type.name().to_string(), header_style),
        ]));

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
            for line in tool.output_display.lines().take(20) {
                lines.push(Line::from(format!("    {}", line)));
            }
            if tool.output_display.lines().count() > 20 {
                lines.push(Line::styled("    ... (truncated)".to_string(), Style::default().fg(Color::DarkGray)));
            }

            lines.push(Line::from(""));
        }
    }

    Text::from(lines)
}

fn render_diffs(turn: &Turn) -> Text<'static> {
    let mut lines: Vec<Line> = Vec::new();
    let mut has_diff = false;

    for tool in &turn.tool_invocations {
        if let Some(diff) = tool.tool_type.diff() {
            has_diff = true;

            // File header
            let path = match &tool.tool_type {
                ToolType::FileEdit { path, .. } => path.clone(),
                ToolType::FileWrite { path, .. } => path.clone(),
                _ => "unknown".to_string(),
            };

            lines.push(Line::styled(
                format!("─── {} ───", path),
                Style::default().fg(Color::Cyan).bold(),
            ));
            lines.push(Line::from(""));

            // Diff content with syntax highlighting
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
        }
    }

    if !has_diff {
        return Text::styled("No diffs available for this turn", Style::default().fg(Color::DarkGray));
    }

    Text::from(lines)
}

// =============================================================================
// Main
// =============================================================================

fn main() -> Result<()> {
    color_eyre::install()?;

    // Setup terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // Create app
    let mut app = App::new();

    // Main loop
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

    // Restore terminal
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}
