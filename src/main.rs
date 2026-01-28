mod claude;
mod codex;
mod models;
mod share;

use std::io;
use std::path::PathBuf;
use std::time::Instant;

use color_eyre::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind},
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
use pulldown_cmark::{Event as MdEvent, Parser, Tag, TagEnd};

use claude::{list_projects, list_sessions, parse_session};
use codex::{list_codex_projects, list_codex_sessions_for_project, parse_codex_session};
use models::{Session, ToolInvocation, ToolType, Turn};

// =============================================================================
// Unified Types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Claude,
    Codex,
}

/// State of the upload operation
#[derive(Debug, Clone)]
pub enum UploadState {
    Idle,
    Confirming,
    Compressing,
    Uploading,
    Complete { url: String },
    Error { message: String },
}

/// A unified session that can come from either source.
#[derive(Debug, Clone)]
pub struct UnifiedSession {
    pub source: Source,
    pub path: PathBuf,
    pub name: String,
    pub project: String,
    pub modified: Option<std::time::SystemTime>,
    pub description: Option<String>,
}

// =============================================================================
// Unified Listing Functions
// =============================================================================

/// List ALL sessions from both Claude and Codex across all projects, sorted by recency.
fn list_all_sessions() -> Vec<UnifiedSession> {
    let mut sessions = Vec::new();

    // Add Claude sessions from all projects
    for claude_project in list_projects() {
        let project_name = PathBuf::from(&claude_project.decoded_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        for session in list_sessions(&claude_project.path) {
            sessions.push(UnifiedSession {
                source: Source::Claude,
                path: session.path,
                name: session.name,
                project: project_name.clone(),
                modified: session.modified,
                description: session.description,
            });
        }
    }

    // Add Codex sessions from all projects
    for codex_project in list_codex_projects() {
        for session in list_codex_sessions_for_project(&codex_project.path) {
            sessions.push(UnifiedSession {
                source: Source::Codex,
                path: session.path,
                name: session.name,
                project: codex_project.name.clone(),
                modified: session.modified,
                description: session.description,
            });
        }
    }

    // Sort by modification time, newest first
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    sessions
}

// =============================================================================
// App State
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
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

/// What was copied to clipboard
#[derive(Debug, Clone)]
pub enum CopySource {
    Tab(String),      // Tab name: "Prompt", "Thinking", "Tool Calls", "Diff"
    Prompt,
    Response,
    Selection,
}

impl std::fmt::Display for CopySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopySource::Tab(name) => write!(f, "{}", name),
            CopySource::Prompt => write!(f, "prompt"),
            CopySource::Response => write!(f, "response"),
            CopySource::Selection => write!(f, "selection"),
        }
    }
}

/// Copy feedback state
#[derive(Debug, Clone)]
pub struct CopyFeedback {
    pub timestamp: Instant,
    pub source: CopySource,
}

/// Text selection state for mouse-based selection
#[derive(Debug, Clone)]
pub struct TextSelection {
    /// Start position (row, col) relative to content area
    pub start: (u16, u16),
    /// End position (row, col) relative to content area
    pub end: (u16, u16),
    /// Whether selection is in progress (mouse is held down)
    pub selecting: bool,
}

impl TextSelection {
    fn new(row: u16, col: u16) -> Self {
        Self {
            start: (row, col),
            end: (row, col),
            selecting: true,
        }
    }

    /// Get normalized selection (start <= end)
    fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if self.start.0 < self.end.0 || (self.start.0 == self.end.0 && self.start.1 <= self.end.1) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

}

pub struct App {
    pub view: View,
    pub sessions: Vec<UnifiedSession>,
    pub session_list_state: ListState,
    // Parsed session state
    pub session: Option<Session>,
    /// Stack of turn contexts (main session at bottom, subagents pushed on top)
    pub context_stack: Vec<TurnContext>,
    pub should_quit: bool,
    pub error_message: Option<String>,
    /// State of the upload operation
    pub upload_state: UploadState,
    /// Copy feedback with source info (clears after 1.5s)
    pub copy_feedback: Option<CopyFeedback>,
    /// Current text selection state
    pub text_selection: Option<TextSelection>,
    /// Content area rect (set during render for mouse hit testing)
    pub content_area: Option<Rect>,
    /// Content lines for text extraction (set during render)
    pub content_lines: Vec<String>,
}

impl App {
    pub fn new() -> Self {
        let sessions = list_all_sessions();
        let mut session_list_state = ListState::default();
        if !sessions.is_empty() {
            session_list_state.select(Some(0));
        }

        Self {
            view: View::SessionBrowser,
            sessions,
            session_list_state,
            session: None,
            context_stack: Vec::new(),
            should_quit: false,
            error_message: None,
            upload_state: UploadState::Idle,
            copy_feedback: None,
            text_selection: None,
            content_area: None,
            content_lines: Vec::new(),
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

        // Handle upload modal first
        if !matches!(self.upload_state, UploadState::Idle) {
            self.handle_upload_key(key);
            return;
        }

        match self.view {
            View::SessionBrowser => self.handle_session_browser_key(key),
            View::SessionViewer => self.handle_session_viewer_key(key),
        }
    }

    fn handle_upload_key(&mut self, key: KeyCode) {
        match &self.upload_state {
            UploadState::Confirming => {
                match key {
                    KeyCode::Enter | KeyCode::Char('y') => {
                        // Start upload
                        self.perform_upload();
                    }
                    KeyCode::Esc | KeyCode::Char('n') => {
                        self.upload_state = UploadState::Idle;
                    }
                    _ => {}
                }
            }
            UploadState::Complete { .. } | UploadState::Error { .. } => {
                // Any key dismisses the result
                self.upload_state = UploadState::Idle;
            }
            _ => {}
        }
    }

    fn perform_upload(&mut self) {
        let session = match &self.session {
            Some(s) => s.clone(),
            None => {
                // Try to load from selected session in browser
                if let Some(i) = self.session_list_state.selected() {
                    if let Some(session_info) = self.sessions.get(i) {
                        let result = match session_info.source {
                            Source::Claude => parse_session(&session_info.path),
                            Source::Codex => parse_codex_session(&session_info.path),
                        };
                        match result {
                            Ok(s) => s,
                            Err(e) => {
                                self.upload_state = UploadState::Error {
                                    message: format!("Failed to parse session: {}", e),
                                };
                                return;
                            }
                        }
                    } else {
                        self.upload_state = UploadState::Error {
                            message: "No session selected".to_string(),
                        };
                        return;
                    }
                } else {
                    self.upload_state = UploadState::Error {
                        message: "No session selected".to_string(),
                    };
                    return;
                }
            }
        };

        self.upload_state = UploadState::Compressing;

        // Compress the session
        let compressed = match share::compress_session(&session) {
            Ok(c) => c,
            Err(e) => {
                self.upload_state = UploadState::Error {
                    message: format!("Compression failed: {}", e),
                };
                return;
            }
        };

        self.upload_state = UploadState::Uploading;

        // Upload to server
        match share::upload_session(&compressed) {
            Ok(response) => {
                // Try to copy URL to clipboard
                let _ = share::copy_to_clipboard(&response.url);
                self.upload_state = UploadState::Complete { url: response.url };
            }
            Err(e) => {
                self.upload_state = UploadState::Error {
                    message: format!("Upload failed: {}", e),
                };
            }
        }
    }

    fn handle_session_browser_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('u') => {
                // Upload selected session
                if self.session_list_state.selected().is_some() {
                    self.upload_state = UploadState::Confirming;
                }
            }
            KeyCode::Up => Self::select_prev_in_list(&mut self.session_list_state, self.sessions.len()),
            KeyCode::Down => Self::select_next_in_list(&mut self.session_list_state, self.sessions.len()),
            KeyCode::Enter => {
                if let Some(i) = self.session_list_state.selected() {
                    if let Some(session_info) = self.sessions.get(i) {
                        let result = match session_info.source {
                            Source::Claude => parse_session(&session_info.path),
                            Source::Codex => parse_codex_session(&session_info.path),
                        };
                        match result {
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
            KeyCode::Char('S') => {
                // Share current session
                if self.session.is_some() {
                    self.upload_state = UploadState::Confirming;
                }
            }
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
            KeyCode::Char('c') => {
                // Copy current tab content
                let tab_name = self.current_tab_name();
                let content = self.get_copyable_content();
                self.copy_to_clipboard(content, CopySource::Tab(tab_name));
            }
            KeyCode::Char('p') => {
                // Copy user prompt only
                let content = self.get_prompt_text();
                self.copy_to_clipboard(content, CopySource::Prompt);
            }
            KeyCode::Char('r') => {
                // Copy response only
                let content = self.get_response_text();
                self.copy_to_clipboard(content, CopySource::Response);
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

    /// Get the copyable text for the current tab
    fn get_copyable_content(&self) -> Option<String> {
        let ctx = self.current_context()?;
        let turn = ctx.selected_turn()?;

        match ctx.active_tab {
            DetailTab::Prompt => {
                let mut content = turn.user_prompt.clone();
                if !turn.response.is_empty() {
                    content.push_str("\n\n---\n\n");
                    content.push_str(&turn.response);
                }
                Some(content)
            }
            DetailTab::Thinking => turn.thinking.clone(),
            DetailTab::ToolCalls => {
                ctx.selected_tool().map(|tool| {
                    format!(
                        "Tool: {}\n\nInput:\n{}\n\nOutput:\n{}",
                        tool.tool_type.name(),
                        tool.input_display,
                        tool.output_display
                    )
                })
            }
            DetailTab::Diff => {
                let mut diffs = String::new();
                for tool in &turn.tool_invocations {
                    if let Some(diff) = tool.tool_type.diff() {
                        let path = match &tool.tool_type {
                            ToolType::FileEdit { path, .. } | ToolType::FileWrite { path, .. } => path.clone(),
                            _ => "unknown".to_string(),
                        };
                        diffs.push_str(&format!("--- {} ---\n{}\n\n", path, diff));
                    }
                }
                if diffs.is_empty() {
                    None
                } else {
                    Some(diffs)
                }
            }
        }
    }

    /// Get just the user prompt text
    fn get_prompt_text(&self) -> Option<String> {
        self.current_context()
            .and_then(|ctx| ctx.selected_turn())
            .map(|turn| turn.user_prompt.clone())
    }

    /// Get just the response text
    fn get_response_text(&self) -> Option<String> {
        self.current_context()
            .and_then(|ctx| ctx.selected_turn())
            .filter(|turn| !turn.response.is_empty())
            .map(|turn| turn.response.clone())
    }

    /// Get the current tab name
    fn current_tab_name(&self) -> String {
        self.current_context()
            .map(|ctx| match ctx.active_tab {
                DetailTab::Prompt => "Prompt",
                DetailTab::Thinking => "Thinking",
                DetailTab::ToolCalls => "Tool Calls",
                DetailTab::Diff => "Diff",
            })
            .unwrap_or("content")
            .to_string()
    }

    /// Copy text to clipboard and show feedback
    fn copy_to_clipboard(&mut self, text: Option<String>, source: CopySource) {
        if let Some(text) = text {
            if share::copy_to_clipboard(&text).is_ok() {
                self.copy_feedback = Some(CopyFeedback {
                    timestamp: Instant::now(),
                    source,
                });
            }
        }
    }

    /// Handle mouse events for text selection
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        // Only handle mouse in session viewer with content
        if self.view != View::SessionViewer {
            return;
        }

        let Some(content_area) = self.content_area else {
            return;
        };

        let x = mouse.column;
        let y = mouse.row;

        // Check if mouse is within content area
        let in_content = x >= content_area.x
            && x < content_area.x + content_area.width
            && y >= content_area.y
            && y < content_area.y + content_area.height;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if in_content {
                    // Start selection - position relative to content area
                    let rel_x = x.saturating_sub(content_area.x);
                    let rel_y = y.saturating_sub(content_area.y);
                    self.text_selection = Some(TextSelection::new(rel_y, rel_x));
                } else {
                    self.text_selection = None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(ref mut selection) = self.text_selection {
                    // Update selection end - clamp to content area
                    let rel_x = x.saturating_sub(content_area.x).min(content_area.width.saturating_sub(1));
                    let rel_y = y.saturating_sub(content_area.y).min(content_area.height.saturating_sub(1));
                    selection.end = (rel_y, rel_x);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(ref mut selection) = self.text_selection {
                    selection.selecting = false;
                    // Extract and copy selected text
                    let selected_text = self.extract_selected_text();
                    if let Some(text) = selected_text {
                        if !text.is_empty() {
                            let _ = share::copy_to_clipboard(&text);
                            self.copy_feedback = Some(CopyFeedback {
                                timestamp: Instant::now(),
                                source: CopySource::Selection,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Extract selected text from content_lines based on current selection
    fn extract_selected_text(&self) -> Option<String> {
        let selection = self.text_selection.as_ref()?;
        let ctx = self.current_context()?;

        if self.content_lines.is_empty() {
            return None;
        }

        let ((start_row, start_col), (end_row, end_col)) = selection.normalized();
        let scroll = ctx.scroll_offset as usize;

        let mut result = String::new();

        for (i, rel_row) in (start_row..=end_row).enumerate() {
            let line_idx = scroll + rel_row as usize;
            if line_idx >= self.content_lines.len() {
                break;
            }

            let line = &self.content_lines[line_idx];
            let chars: Vec<char> = line.chars().collect();

            let line_start = if rel_row == start_row { start_col as usize } else { 0 };
            let line_end = if rel_row == end_row {
                (end_col as usize + 1).min(chars.len())
            } else {
                chars.len()
            };

            if line_start < chars.len() {
                let selected: String = chars[line_start..line_end.min(chars.len())].iter().collect();
                result.push_str(&selected);
            }

            if i < (end_row - start_row) as usize {
                result.push('\n');
            }
        }

        Some(result)
    }
}

// =============================================================================
// UI Rendering
// =============================================================================

fn ui(frame: &mut Frame, app: &mut App) {
    match app.view {
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

    // Render upload modal if active
    if !matches!(app.upload_state, UploadState::Idle) {
        render_upload_modal(frame, app);
    }
}

fn render_upload_modal(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Create centered modal area
    let modal_width = 60.min(area.width.saturating_sub(4));
    let modal_height = 10.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width - modal_width) / 2,
        y: (area.height - modal_height) / 2,
        width: modal_width,
        height: modal_height,
    };

    // Clear background
    frame.render_widget(Clear, modal_area);

    let (title, content, style) = match &app.upload_state {
        UploadState::Idle => return,
        UploadState::Confirming => {
            let session_name = app.session.as_ref()
                .map(|s| s.name.clone())
                .or_else(|| {
                    app.session_list_state.selected()
                        .and_then(|i| app.sessions.get(i))
                        .map(|s| s.name.clone())
                })
                .unwrap_or_else(|| "Unknown".to_string());
            (
                " Share Session ",
                format!(
                    "Share \"{}\"?\n\n\
                    This will upload the session to the cloud and\n\
                    create a shareable link.\n\n\
                    Press Enter or 'y' to confirm, Esc or 'n' to cancel",
                    truncate_str(&session_name, 30)
                ),
                Style::default().fg(Color::Yellow),
            )
        }
        UploadState::Compressing => (
            " Sharing... ",
            "Compressing session...".to_string(),
            Style::default().fg(Color::Cyan),
        ),
        UploadState::Uploading => (
            " Sharing... ",
            "Uploading to cloud...".to_string(),
            Style::default().fg(Color::Cyan),
        ),
        UploadState::Complete { url } => (
            " Share Complete ",
            format!(
                "Session shared successfully!\n\n\
                URL: {}\n\n\
                (Copied to clipboard)\n\n\
                Press any key to close",
                url
            ),
            Style::default().fg(Color::Green),
        ),
        UploadState::Error { message } => (
            " Share Failed ",
            format!(
                "Error: {}\n\n\
                Press any key to close",
                message
            ),
            Style::default().fg(Color::Red),
        ),
    };

    let modal = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(style)
                .title(title)
                .title_style(style.add_modifier(Modifier::BOLD)),
        )
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::White));

    frame.render_widget(modal, modal_area);
}

fn render_session_browser(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Column widths:
    // ID: 16 chars (e.g., "claude:063cd168")
    // TIME: 8 chars (e.g., "12h ago")
    // SOURCE: 6 chars (e.g., "claude")
    // PROJECT: 16 chars
    // DESCRIPTION: remaining
    let id_width = 16;
    let time_width = 8;
    let source_width = 6;
    let project_width = 16;
    // borders(2) + highlight(2) + spacing(4 separators * 2 = 8) = 12
    let desc_width = (area.width as usize).saturating_sub(12 + id_width + time_width + source_width + project_width).max(10);

    let title = format!(" Sessions ({}) - Enter: open | u: share | q: quit ", app.sessions.len());

    // Render header line and list
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(area);

    let header = format!(
        "  {:<id_w$}  {:<time_w$}  {:<src_w$}  {:<proj_w$}  {}",
        "ID", "TIME", "SOURCE", "PROJECT", "DESCRIPTION",
        id_w = id_width,
        time_w = time_width,
        src_w = source_width,
        proj_w = project_width
    );
    let header_para = Paragraph::new(header)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::TOP | Borders::LEFT | Borders::RIGHT).title(title));
    frame.render_widget(header_para, chunks[0]);

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let id = match s.source {
                Source::Claude => format!("claude:{}", &s.name[..8.min(s.name.len())]),
                Source::Codex => format!("codex:{}", &s.name[..8.min(s.name.len())]),
            };
            let time = format_time_ago(s.modified);
            let source = match s.source {
                Source::Claude => "claude",
                Source::Codex => "codex",
            };
            let project = truncate_str(&s.project, project_width);
            let desc = s.description.as_deref().unwrap_or(&s.name);
            let desc_display = truncate_str(desc, desc_width);

            let display = format!(
                "{:<id_w$}  {:<time_w$}  {:<src_w$}  {:<proj_w$}  {}",
                id,
                time,
                source,
                project,
                desc_display,
                id_w = id_width,
                time_w = time_width,
                src_w = source_width,
                proj_w = project_width
            );
            ListItem::new(display)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, chunks[1], &mut app.session_list_state);
}

fn format_time_ago(modified: Option<std::time::SystemTime>) -> String {
    modified
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
        .unwrap_or_default()
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

fn render_detail_panel(frame: &mut Frame, app: &mut App, area: Rect) {
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

    // Store the inner content area (excluding 1-char border on each side)
    let inner_content_area = Rect {
        x: content_area.x + 1,
        y: content_area.y + 1,
        width: content_area.width.saturating_sub(2),
        height: content_area.height.saturating_sub(2),
    };

    if let Some(turn) = ctx.selected_turn() {
        let content: Text = match ctx.active_tab {
            DetailTab::Prompt => render_prompt_tab(turn),
            DetailTab::Thinking => render_thinking_tab(turn),
            DetailTab::ToolCalls => render_tool_calls_tab(turn, ctx.tool_scroll_offset),
            DetailTab::Diff => render_diff_tab(turn),
        };

        // Extract plain text lines for selection
        let content_lines: Vec<String> = content.lines.iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();

        // Apply selection highlighting if active
        let content = apply_selection_highlight(content, &app.text_selection, ctx.scroll_offset, inner_content_area.width);

        let paragraph = Paragraph::new(content)
            .block(content_block)
            .wrap(Wrap { trim: false })
            .scroll((ctx.scroll_offset, 0));
        frame.render_widget(paragraph, content_area);

        // Store for mouse handling (after we're done with ctx borrow)
        app.content_area = Some(inner_content_area);
        app.content_lines = content_lines;
    } else {
        let paragraph = Paragraph::new("Select a turn to view details")
            .block(content_block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, content_area);
        app.content_area = None;
        app.content_lines.clear();
    }

    // Help line - show "Copied!" feedback briefly, otherwise show help
    let copy_feedback = app.copy_feedback.as_ref()
        .filter(|f| f.timestamp.elapsed().as_millis() < 1500);

    let (help_text, help_style) = if let Some(feedback) = copy_feedback {
        (
            format!(" ✓ Copied {} to clipboard! ", feedback.source),
            Style::default().fg(Color::Green),
        )
    } else if app.is_subagent_view() {
        (
            " ↑/↓: Navigate | ←/→: Tabs | j/k: Scroll | c/p/r: Copy | Mouse: Select | Esc: Back | q: Quit ".to_string(),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            " ↑/↓: Navigate | ←/→: Tabs | j/k: Scroll | c/p/r: Copy | Mouse: Select | Enter: Open | q: Quit ".to_string(),
            Style::default().fg(Color::DarkGray),
        )
    };

    let help = Paragraph::new(help_text).style(help_style);
    frame.render_widget(help, help_area);
}

/// Apply selection highlighting to text content
fn apply_selection_highlight(content: Text<'static>, selection: &Option<TextSelection>, scroll_offset: u16, _width: u16) -> Text<'static> {
    let Some(sel) = selection else {
        return content;
    };

    let ((start_row, start_col), (end_row, end_col)) = sel.normalized();

    // Convert selection coordinates (relative to visible area) to content line indices
    let sel_start_line = scroll_offset as usize + start_row as usize;
    let sel_end_line = scroll_offset as usize + end_row as usize;

    let highlight_style = Style::default().bg(Color::Blue).fg(Color::White);

    let mut new_lines: Vec<Line<'static>> = Vec::new();

    for (line_idx, line) in content.lines.into_iter().enumerate() {
        if line_idx < sel_start_line || line_idx > sel_end_line {
            // Line not in selection
            new_lines.push(line);
            continue;
        }

        // This line is (partially) selected
        let mut new_spans: Vec<Span<'static>> = Vec::new();
        let mut current_col: usize = 0;

        for span in line.spans {
            let span_text: String = span.content.to_string();
            let span_len = span_text.chars().count();
            let span_end_col = current_col + span_len;

            // Determine selection range within this line
            let line_sel_start = if line_idx == sel_start_line { start_col as usize } else { 0 };
            let line_sel_end = if line_idx == sel_end_line { end_col as usize + 1 } else { usize::MAX };

            // Check if this span overlaps with selection
            if span_end_col <= line_sel_start || current_col >= line_sel_end {
                // Span is outside selection
                new_spans.push(Span::styled(span_text, span.style));
            } else {
                // Span overlaps with selection - split it
                let chars: Vec<char> = span_text.chars().collect();

                // Part before selection
                if current_col < line_sel_start {
                    let before: String = chars[..line_sel_start - current_col].iter().collect();
                    new_spans.push(Span::styled(before, span.style));
                }

                // Selected part
                let sel_start_in_span = line_sel_start.saturating_sub(current_col);
                let sel_end_in_span = (line_sel_end - current_col).min(span_len);
                if sel_start_in_span < span_len {
                    let selected: String = chars[sel_start_in_span..sel_end_in_span].iter().collect();
                    new_spans.push(Span::styled(selected, highlight_style));
                }

                // Part after selection
                if line_sel_end < span_end_col {
                    let after: String = chars[line_sel_end - current_col..].iter().collect();
                    new_spans.push(Span::styled(after, span.style));
                }
            }

            current_col = span_end_col;
        }

        new_lines.push(Line::from(new_spans));
    }

    Text::from(new_lines)
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

/// Render markdown text to Ratatui Lines
fn render_markdown(md: &str) -> Vec<Line<'static>> {
    let parser = Parser::new(md);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    // Style state stack
    let mut is_bold = false;
    let mut is_italic = false;
    let is_code = false;
    let mut is_heading = false;
    let mut in_code_block = false;

    for event in parser {
        match event {
            MdEvent::Start(tag) => match tag {
                Tag::Heading { .. } => is_heading = true,
                Tag::Strong => is_bold = true,
                Tag::Emphasis => is_italic = true,
                Tag::CodeBlock(_) => in_code_block = true,
                Tag::List(_) | Tag::Item => {}
                _ => {}
            },
            MdEvent::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    is_heading = false;
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                TagEnd::Strong => is_bold = false,
                TagEnd::Emphasis => is_italic = false,
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                TagEnd::Paragraph => {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                    lines.push(Line::from(""));
                }
                TagEnd::Item => {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                _ => {}
            },
            MdEvent::Text(text) => {
                let style = compute_style(is_bold, is_italic, is_code || in_code_block, is_heading);
                for (i, line_text) in text.lines().enumerate() {
                    if i > 0 {
                        lines.push(Line::from(std::mem::take(&mut current_spans)));
                    }
                    if !line_text.is_empty() {
                        current_spans.push(Span::styled(line_text.to_string(), style));
                    }
                }
            }
            MdEvent::Code(code) => {
                let style = Style::default().bg(Color::Indexed(236)).fg(Color::Green);
                current_spans.push(Span::styled(code.to_string(), style));
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                lines.push(Line::from(std::mem::take(&mut current_spans)));
            }
            _ => {}
        }
    }

    // Flush remaining spans
    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }

    // Remove trailing empty lines
    while lines.last().map_or(false, |l| l.spans.is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
    }

    lines
}

/// Compute style based on current state
fn compute_style(bold: bool, italic: bool, code: bool, heading: bool) -> Style {
    let mut style = Style::default();

    if heading {
        style = style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
    } else if code {
        style = style.bg(Color::Indexed(236)).fg(Color::Green);
    } else {
        if bold {
            style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
        }
        if italic {
            style = style.fg(Color::Magenta).add_modifier(Modifier::ITALIC);
        }
    }

    style
}

fn render_prompt_tab(turn: &Turn) -> Text<'static> {
    let mut lines = vec![
        Line::styled("User Prompt:".to_string(), Style::default().fg(Color::Cyan).bold()),
        Line::from(""),
    ];

    // User prompt as plain text (don't interpret as markdown)
    for line in turn.user_prompt.lines() {
        lines.push(Line::from(line.to_string()));
    }

    if !turn.response.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled("─".repeat(40), Style::default().fg(Color::DarkGray)));
        lines.push(Line::from(""));
        lines.push(Line::styled("Response:".to_string(), Style::default().fg(Color::Green).bold()));
        lines.push(Line::from(""));
        // Render response as markdown
        lines.extend(render_markdown(&turn.response));
    }

    Text::from(lines)
}

fn render_thinking_tab(turn: &Turn) -> Text<'static> {
    if let Some(thinking) = &turn.thinking {
        let mut lines = vec![
            Line::styled("Model Thinking:".to_string(), Style::default().fg(Color::Magenta).bold()),
            Line::from(""),
        ];
        // Render thinking as markdown
        lines.extend(render_markdown(thinking));
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
    io::stdout().execute(EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = App::new();

    while !app.should_quit {
        terminal.draw(|frame| ui(frame, &mut app))?;

        if event::poll(std::time::Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        app.handle_key(key.code);
                    }
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse);
                }
                _ => {}
            }
        }
    }

    io::stdout().execute(DisableMouseCapture)?;
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}
