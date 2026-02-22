mod auth;
mod claude;
mod codex;
mod models;
mod share;

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use color_eyre::{eyre::eyre, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEvent, MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use pulldown_cmark::{Event as MdEvent, Parser, Tag, TagEnd};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};

use claude::{list_projects, list_sessions, parse_session};
use codex::{list_codex_projects, list_codex_sessions_for_project, parse_codex_session};
use models::{Session, ToolInvocation, ToolType, Turn};

// =============================================================================
// Constants
// =============================================================================

/// Background color for search match highlights (dark gray)
const SEARCH_HIGHLIGHT_BG: Color = Color::Indexed(238);
/// Background color for the current/active search match
const SEARCH_CURRENT_BG: Color = Color::Yellow;
/// Foreground color for the current/active search match
const SEARCH_CURRENT_FG: Color = Color::Black;
const COPY_FEEDBACK_DURATION: Duration = Duration::from_millis(1500);
const UPLOAD_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(40);
const IDLE_POLL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
enum CliCommand {
    Browse,
    Import(PathBuf),
    Login,
    Uploads,
    Help,
}

// =============================================================================
// Unified Types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareTarget {
    File,
    Cloud,
}

impl ShareTarget {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::File => "File export",
            Self::Cloud => "Cloud link",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudShareSecurity {
    Encrypted,
    Public,
}

impl CloudShareSecurity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Encrypted => "Encrypted (recommended)",
            Self::Public => "Public (not encrypted)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareDialogSection {
    Destination,
    CloudSecurity,
    ExportMode,
}

fn confirming_sections(cloud_available: bool, target: ShareTarget) -> Vec<ShareDialogSection> {
    if !cloud_available {
        return vec![ShareDialogSection::ExportMode];
    }
    match target {
        ShareTarget::Cloud => vec![
            ShareDialogSection::Destination,
            ShareDialogSection::CloudSecurity,
        ],
        ShareTarget::File => vec![
            ShareDialogSection::Destination,
            ShareDialogSection::ExportMode,
        ],
    }
}

fn normalize_confirming_focus(
    focus: ShareDialogSection,
    cloud_available: bool,
    target: ShareTarget,
) -> ShareDialogSection {
    let sections = confirming_sections(cloud_available, target);
    if sections.contains(&focus) {
        focus
    } else {
        sections[0]
    }
}

fn move_confirming_focus(
    focus: ShareDialogSection,
    cloud_available: bool,
    target: ShareTarget,
    forward: bool,
) -> ShareDialogSection {
    let sections = confirming_sections(cloud_available, target);
    let current = sections.iter().position(|s| *s == focus).unwrap_or(0);
    let next = if forward {
        (current + 1) % sections.len()
    } else {
        (current + sections.len() - 1) % sections.len()
    };
    sections[next]
}

fn adjust_confirming_selection(
    target: &mut ShareTarget,
    mode: &mut share::ShareExportMode,
    cloud_security: &mut CloudShareSecurity,
    focus: ShareDialogSection,
    cloud_available: bool,
    forward: bool,
) {
    match focus {
        ShareDialogSection::Destination => {
            if cloud_available {
                *target = match *target {
                    ShareTarget::Cloud => ShareTarget::File,
                    ShareTarget::File => ShareTarget::Cloud,
                };
            }
        }
        ShareDialogSection::CloudSecurity => {
            *cloud_security = match *cloud_security {
                CloudShareSecurity::Encrypted => CloudShareSecurity::Public,
                CloudShareSecurity::Public => CloudShareSecurity::Encrypted,
            };
        }
        ShareDialogSection::ExportMode => {
            const MODES: [share::ShareExportMode; 3] = [
                share::ShareExportMode::PromptResponseOnly,
                share::ShareExportMode::PromptResponseAndDiff,
                share::ShareExportMode::FullSession,
            ];
            let current = MODES.iter().position(|m| *m == *mode).unwrap_or(0);
            let next = if forward {
                (current + 1) % MODES.len()
            } else {
                (current + MODES.len() - 1) % MODES.len()
            };
            *mode = MODES[next];
        }
    }
}

fn default_confirming_focus(cloud_available: bool, target: ShareTarget) -> ShareDialogSection {
    confirming_sections(cloud_available, target)[0]
}

/// State of the upload operation
#[derive(Debug, Clone)]
pub enum UploadState {
    Idle,
    Confirming {
        target: ShareTarget,
        mode: share::ShareExportMode,
        cloud_api_url: Option<String>,
        cloud_security: CloudShareSecurity,
        focus: ShareDialogSection,
    },
    SecretScanning,
    SecretConfirming {
        findings: Vec<share::SecretScanFinding>,
    },
    Compressing {
        target: ShareTarget,
    },
    Uploading {
        target: ShareTarget,
    },
    Complete {
        target: ShareTarget,
        location: String,
        resumable: bool,
        cloud_security: Option<CloudShareSecurity>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug)]
struct PendingCloudUpload {
    session: Session,
    api_url: String,
    auth_token: String,
    cloud_security: CloudShareSecurity,
}

#[derive(Debug)]
struct FileExportResult {
    location: String,
    resumable: bool,
}

#[derive(Debug)]
struct CloudUploadResult {
    location: String,
    cloud_security: CloudShareSecurity,
    reused: bool,
}

#[derive(Debug, Clone, Copy)]
enum UploadWorkerProgress {
    Compressing(ShareTarget),
    Uploading(ShareTarget),
}

#[derive(Debug)]
enum UploadWorkerMessage {
    SecretScanFinished {
        findings: Vec<share::SecretScanFinding>,
    },
    Progress(UploadWorkerProgress),
    FileExportFinished {
        result: std::result::Result<FileExportResult, String>,
    },
    CloudUploadFinished {
        result: std::result::Result<CloudUploadResult, String>,
    },
}

#[derive(Debug, Clone)]
pub enum ResumeState {
    Idle,
    /// Confirming resume with the command to be copied
    Confirming {
        command: String,
    },
    /// Resume command copied to clipboard
    Complete {
        command: String,
    },
}

#[derive(Debug, Clone)]
pub enum TurnMetadataState {
    Idle,
    Showing {
        session_name: String,
        content: String,
    },
}

/// A unified session that can come from either source.
/// Claude sessions with the same slug in the same project are grouped together.
#[derive(Debug, Clone)]
pub struct UnifiedSession {
    pub source: Source,
    /// All session file paths (grouped by slug, sorted oldest to newest)
    pub paths: Vec<PathBuf>,
    /// Display name (first session ID or slug)
    pub name: String,
    /// Short project name for display
    pub project: String,
    /// Full project path for resume command
    pub project_path: PathBuf,
    pub modified: Option<std::time::SystemTime>,
    pub description: Option<String>,
    /// Session slug (used for grouping continuations)
    pub slug: Option<String>,
    /// Number of session files in this group
    pub part_count: usize,
}

impl UnifiedSession {
    const LIST_ID_CHARS: usize = 12;

    /// Get the short ID shown in the session browser list.
    #[must_use]
    pub fn list_id(&self) -> String {
        let base_id = match self.source {
            Source::Claude => self.name.clone(),
            Source::Codex => {
                let raw = self.resume_session_id();
                raw.strip_prefix("rollout-").unwrap_or(&raw).to_string()
            }
        };
        let short = base_id[..Self::LIST_ID_CHARS.min(base_id.len())].to_string();
        if self.part_count > 1 {
            format!("{short}×{}", self.part_count)
        } else {
            short
        }
    }

    /// Get the session ID to use for resuming (most recent session file).
    /// For Claude: returns the full filename stem (UUID)
    /// For Codex: extracts just the UUID from "rollout-DATE-UUID" format
    #[must_use]
    pub fn resume_session_id(&self) -> String {
        let filename = self
            .paths
            .last()
            .and_then(|p| p.file_stem())
            .map_or_else(|| self.name.clone(), |s| s.to_string_lossy().to_string());

        match self.source {
            Source::Claude => filename,
            Source::Codex => {
                // Codex filenames are like "rollout-2026-01-27T18-20-41-019c0267-28af-7ae2-9531-008179fabf86"
                // Extract just the UUID (last 5 hyphen-separated segments: 8-4-4-4-12)
                let parts: Vec<&str> = filename.rsplitn(6, '-').collect();
                if parts.len() >= 5 {
                    // parts are reversed, so join them back in correct order
                    format!(
                        "{}-{}-{}-{}-{}",
                        parts[4], parts[3], parts[2], parts[1], parts[0]
                    )
                } else {
                    filename
                }
            }
        }
    }

    /// Get the command to resume this session.
    #[must_use]
    pub fn get_resume_command(&self) -> String {
        let session_id = self.resume_session_id();
        let project_path = self.project_path.display();

        match self.source {
            Source::Claude => {
                format!("cd {project_path} && claude --resume {session_id}")
            }
            Source::Codex => {
                format!("cd {project_path} && codex resume {session_id}")
            }
        }
    }

    /// Parse all session files and combine turns into a single Session.
    /// For grouped sessions (multiple paths), turns from all parts are combined chronologically.
    pub fn parse(&self) -> Result<Session, String> {
        match self.source {
            Source::Claude => {
                if self.paths.len() == 1 {
                    parse_session(&self.paths[0])
                } else {
                    // Parse all parts and combine turns
                    let mut combined_turns: Vec<Turn> = Vec::new();
                    let mut session_name = self.name.clone();
                    let mut project_path = None;

                    for path in &self.paths {
                        match parse_session(path) {
                            Ok(session) => {
                                if project_path.is_none() {
                                    project_path = session.project_path.clone();
                                }
                                // Use slug as name if available
                                if self.slug.is_some() {
                                    session_name = self.slug.clone().unwrap_or(session.name);
                                }
                                combined_turns.extend(session.turns);
                            }
                            Err(e) => {
                                // Log error but continue with other parts
                                eprintln!("Warning: Failed to parse {}: {}", path.display(), e);
                            }
                        }
                    }

                    Ok(Session {
                        id: self.name.clone(),
                        name: session_name,
                        source: models::SessionSource::ClaudeCode { version: None },
                        project_path,
                        turns: combined_turns,
                    })
                }
            }
            Source::Codex => {
                // Codex sessions are not grouped
                parse_codex_session(&self.paths[0])
            }
        }
    }
}

// =============================================================================
// Unified Listing Functions
// =============================================================================

/// Intermediate struct for collecting session parts before grouping
struct SessionPart {
    source: Source,
    path: PathBuf,
    name: String,
    project: String,
    project_path: PathBuf,
    modified: Option<std::time::SystemTime>,
    description: Option<String>,
    slug: Option<String>,
}

/// List ALL sessions from both Claude and Codex across all projects, sorted by recency.
/// Claude sessions with the same slug in the same project are grouped together.
fn list_all_sessions() -> Vec<UnifiedSession> {
    let mut parts: Vec<SessionPart> = Vec::new();

    // Collect Claude sessions from all projects
    for claude_project in list_projects() {
        let project_path = PathBuf::from(&claude_project.decoded_path);
        let project_name = project_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        for session in list_sessions(&claude_project.path) {
            parts.push(SessionPart {
                source: Source::Claude,
                path: session.path,
                name: session.name,
                project: project_name.clone(),
                project_path: project_path.clone(),
                modified: session.modified,
                description: session.description,
                slug: session.slug,
            });
        }
    }

    // Collect Codex sessions from all projects
    for codex_project in list_codex_projects() {
        for session in list_codex_sessions_for_project(&codex_project.path) {
            parts.push(SessionPart {
                source: Source::Codex,
                path: session.path,
                name: session.name,
                project: codex_project.name.clone(),
                project_path: codex_project.path.clone(),
                modified: session.modified,
                description: session.description,
                slug: None, // Codex doesn't have slugs
            });
        }
    }

    group_session_parts(parts)
}

/// Group session parts into unified sessions.
/// Claude continuations are grouped by `(project_path, slug)` to avoid
/// cross-project slug collisions.
fn group_session_parts(parts: Vec<SessionPart>) -> Vec<UnifiedSession> {
    use std::collections::HashMap;

    let mut slug_groups: HashMap<(PathBuf, String), Vec<SessionPart>> = HashMap::new();
    let mut ungrouped: Vec<SessionPart> = Vec::new();

    for part in parts {
        if part.source == Source::Claude {
            if let Some(slug) = part.slug.clone() {
                slug_groups
                    .entry((part.project_path.clone(), slug))
                    .or_default()
                    .push(part);
                continue;
            }
        }
        ungrouped.push(part);
    }

    let mut sessions: Vec<UnifiedSession> = Vec::new();

    for ((_project_path, slug), mut group) in slug_groups {
        // Oldest -> newest, so continuation order is stable.
        group.sort_by(|a, b| a.modified.cmp(&b.modified));

        let part_count = group.len();
        let paths: Vec<PathBuf> = group.iter().map(|p| p.path.clone()).collect();
        let latest = group.last().unwrap();
        let description = group
            .iter()
            .find_map(|p| p.description.clone())
            .or_else(|| latest.description.clone());

        sessions.push(UnifiedSession {
            source: latest.source,
            paths,
            name: group.first().unwrap().name.clone(),
            project: latest.project.clone(),
            project_path: latest.project_path.clone(),
            modified: latest.modified,
            description,
            slug: Some(slug),
            part_count,
        });
    }

    for part in ungrouped {
        sessions.push(UnifiedSession {
            source: part.source,
            paths: vec![part.path],
            name: part.name,
            project: part.project,
            project_path: part.project_path,
            modified: part.modified,
            description: part.description,
            slug: None,
            part_count: 1,
        });
    }

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
    const fn next(self) -> Self {
        match self {
            Self::Prompt => Self::Thinking,
            Self::Thinking => Self::ToolCalls,
            Self::ToolCalls => Self::Diff,
            Self::Diff => Self::Prompt,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Prompt => Self::Diff,
            Self::Thinking => Self::Prompt,
            Self::ToolCalls => Self::Thinking,
            Self::Diff => Self::ToolCalls,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Prompt => 0,
            Self::Thinking => 1,
            Self::ToolCalls => 2,
            Self::Diff => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    SessionList,
    TurnList,
    Content,
    Diff,
}

impl std::fmt::Display for SearchScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionList => write!(f, "sessions"),
            Self::TurnList => write!(f, "turns"),
            Self::Content => write!(f, "content"),
            Self::Diff => write!(f, "diff"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub struct SearchState {
    pub scope: SearchScope,
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub cursor: usize,
    pub lines: Vec<String>,
    pub committed: bool,
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
        self.turn_list_state
            .selected()
            .and_then(|i| self.turns.get(i))
    }

    fn selected_tool(&self) -> Option<&ToolInvocation> {
        self.selected_turn()
            .and_then(|t| t.tool_invocations.get(self.tool_scroll_offset))
    }
}

/// What was copied to clipboard
#[derive(Debug, Clone)]
pub enum CopySource {
    Tab(String), // Tab name: "Prompt", "Thinking", "Tool Calls", "Diff"
    Selection,
}

impl std::fmt::Display for CopySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tab(name) => write!(f, "{name}"),
            Self::Selection => write!(f, "selection"),
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
    const fn new(row: u16, col: u16) -> Self {
        Self {
            start: (row, col),
            end: (row, col),
            selecting: true,
        }
    }

    /// Get normalized selection (start <= end)
    const fn normalized(&self) -> ((u16, u16), (u16, u16)) {
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
    /// Index of the currently viewed session in sessions list (for resume command)
    pub current_session_index: Option<usize>,
    /// Stack of turn contexts (main session at bottom, subagents pushed on top)
    pub context_stack: Vec<TurnContext>,
    pub should_quit: bool,
    pub error_message: Option<String>,
    /// State of the upload operation
    pub upload_state: UploadState,
    /// State of the resume operation
    pub resume_state: ResumeState,
    /// State of the per-turn model/effort debug view
    pub turn_metadata_state: TurnMetadataState,
    /// Copy feedback with source info (clears after 1.5s)
    pub copy_feedback: Option<CopyFeedback>,
    /// Current text selection state
    pub text_selection: Option<TextSelection>,
    /// Content area rect (set during render for mouse hit testing)
    pub content_area: Option<Rect>,
    /// Content lines for text extraction (set during render)
    pub content_lines: Vec<String>,
    /// Scroll offset associated with `content_lines` for selection mapping
    pub selection_scroll_offset: u16,
    /// Search state (active when Some)
    pub search: Option<SearchState>,
    upload_worker_rx: Option<Receiver<UploadWorkerMessage>>,
    pending_cloud_upload: Option<PendingCloudUpload>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    #[must_use]
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
            current_session_index: None,
            context_stack: Vec::new(),
            should_quit: false,
            error_message: None,
            upload_state: UploadState::Idle,
            resume_state: ResumeState::Idle,
            turn_metadata_state: TurnMetadataState::Idle,
            copy_feedback: None,
            text_selection: None,
            content_area: None,
            content_lines: Vec::new(),
            selection_scroll_offset: 0,
            search: None,
            upload_worker_rx: None,
            pending_cloud_upload: None,
        }
    }

    #[must_use]
    pub fn with_open_session(session: Session) -> Self {
        let mut app = Self::new();
        let context = TurnContext::new(session.name.clone(), session.turns.clone());
        app.view = View::SessionViewer;
        app.session = Some(session);
        app.current_session_index = None;
        app.context_stack = vec![context];
        app
    }

    /// Get the current (top) context
    #[must_use]
    pub fn current_context(&self) -> Option<&TurnContext> {
        self.context_stack.last()
    }

    /// Get mutable reference to current context
    pub fn current_context_mut(&mut self) -> Option<&mut TurnContext> {
        self.context_stack.last_mut()
    }

    /// Check if we're in a subagent view (depth > 1)
    #[must_use]
    pub const fn is_subagent_view(&self) -> bool {
        self.context_stack.len() > 1
    }

    /// Get the breadcrumb path
    #[must_use]
    pub fn breadcrumb(&self) -> String {
        self.context_stack
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>()
            .join(" > ")
    }

    #[must_use]
    fn next_timer_deadline(&self) -> Option<Instant> {
        let copy_deadline = self
            .copy_feedback
            .as_ref()
            .map(|f| f.timestamp + COPY_FEEDBACK_DURATION);

        if self.upload_worker_rx.is_none() {
            return copy_deadline;
        }

        let worker_deadline = Instant::now() + UPLOAD_WORKER_POLL_INTERVAL;
        Some(match copy_deadline {
            Some(deadline) => deadline.min(worker_deadline),
            None => worker_deadline,
        })
    }

    fn process_timers(&mut self) -> bool {
        let mut changed = false;
        if self
            .copy_feedback
            .as_ref()
            .is_some_and(|f| f.timestamp.elapsed() >= COPY_FEEDBACK_DURATION)
        {
            self.copy_feedback = None;
            changed = true;
        }
        if self.process_upload_worker_message() {
            changed = true;
        }
        changed
    }

    fn process_upload_worker_message(&mut self) -> bool {
        let recv_result = match self.upload_worker_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return false,
        };

        match recv_result {
            Ok(msg) => {
                self.handle_upload_worker_message(msg);
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.upload_worker_rx = None;
                if matches!(
                    self.upload_state,
                    UploadState::SecretScanning
                        | UploadState::Compressing { .. }
                        | UploadState::Uploading { .. }
                ) {
                    self.pending_cloud_upload = None;
                    self.upload_state = UploadState::Error {
                        message: "Share worker stopped unexpectedly.".to_string(),
                    };
                    true
                } else {
                    false
                }
            }
        }
    }

    fn handle_upload_worker_message(&mut self, msg: UploadWorkerMessage) {
        match msg {
            UploadWorkerMessage::SecretScanFinished { findings } => {
                self.upload_worker_rx = None;
                let Some(pending) = self.pending_cloud_upload.take() else {
                    self.upload_state = UploadState::Error {
                        message: "Cloud upload context expired. Retry sharing.".to_string(),
                    };
                    return;
                };

                if findings.is_empty() {
                    self.start_cloud_upload_worker(pending);
                } else {
                    self.pending_cloud_upload = Some(pending);
                    self.upload_state = UploadState::SecretConfirming { findings };
                }
            }
            UploadWorkerMessage::Progress(progress) => match progress {
                UploadWorkerProgress::Compressing(target) => {
                    self.upload_state = UploadState::Compressing { target };
                }
                UploadWorkerProgress::Uploading(target) => {
                    self.upload_state = UploadState::Uploading { target };
                }
            },
            UploadWorkerMessage::FileExportFinished { result } => {
                self.upload_worker_rx = None;
                match result {
                    Ok(done) => {
                        let _ = share::copy_to_clipboard(&done.location);
                        self.upload_state = UploadState::Complete {
                            target: ShareTarget::File,
                            location: done.location,
                            resumable: done.resumable,
                            cloud_security: None,
                        };
                    }
                    Err(message) => {
                        self.upload_state = UploadState::Error { message };
                    }
                }
            }
            UploadWorkerMessage::CloudUploadFinished { result } => {
                self.upload_worker_rx = None;
                self.pending_cloud_upload = None;
                match result {
                    Ok(done) => {
                        let _ = share::copy_to_clipboard(&done.location);
                        self.upload_state = UploadState::Complete {
                            target: ShareTarget::Cloud,
                            location: done.location,
                            resumable: false,
                            cloud_security: Some(done.cloud_security),
                        };
                        if done.reused {
                            self.error_message = Some(
                                "Reused existing cloud link for this unchanged session."
                                    .to_string(),
                            );
                        }
                    }
                    Err(message) => {
                        self.upload_state = UploadState::Error { message };
                    }
                }
            }
        }
    }

    fn select_next_in_list(state: &mut ListState, len: usize) {
        if len == 0 {
            return;
        }
        let i = match state.selected() {
            Some(i) => {
                if i >= len - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        state.select(Some(i));
    }

    fn select_prev_in_list(state: &mut ListState, len: usize) {
        if len == 0 {
            return;
        }
        let i = match state.selected() {
            Some(i) => {
                if i == 0 {
                    len - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        state.select(Some(i));
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        self.error_message = None;

        if self.search.is_some() {
            self.handle_search_key(key);
            return;
        }

        // Handle modals first
        if !matches!(self.upload_state, UploadState::Idle) {
            self.handle_upload_key(key);
            return;
        }

        if !matches!(self.resume_state, ResumeState::Idle) {
            self.handle_resume_key(key);
            return;
        }

        if !matches!(self.turn_metadata_state, TurnMetadataState::Idle) {
            self.handle_turn_metadata_key(key);
            return;
        }

        match self.view {
            View::SessionBrowser => self.handle_session_browser_key(key),
            View::SessionViewer => self.handle_session_viewer_key(key),
        }
    }

    fn handle_upload_key(&mut self, key: KeyCode) {
        match self.upload_state.clone() {
            UploadState::Idle => {}
            UploadState::Confirming {
                mut target,
                mut mode,
                cloud_api_url,
                mut cloud_security,
                mut focus,
            } => {
                let cloud_available = cloud_api_url.is_some();
                focus = normalize_confirming_focus(focus, cloud_available, target);

                match key {
                    KeyCode::Char('c') => {
                        if cloud_available {
                            target = ShareTarget::Cloud;
                        }
                    }
                    KeyCode::Char('f') => target = ShareTarget::File,
                    KeyCode::Char('1') => mode = share::ShareExportMode::PromptResponseOnly,
                    KeyCode::Char('2') => mode = share::ShareExportMode::PromptResponseAndDiff,
                    KeyCode::Char('3') => mode = share::ShareExportMode::FullSession,
                    KeyCode::Char('e') => cloud_security = CloudShareSecurity::Encrypted,
                    KeyCode::Char('p') => cloud_security = CloudShareSecurity::Public,
                    KeyCode::Tab => {
                        focus = move_confirming_focus(focus, cloud_available, target, true);
                    }
                    KeyCode::BackTab => {
                        focus = move_confirming_focus(focus, cloud_available, target, false);
                    }
                    KeyCode::Left | KeyCode::Up => {
                        adjust_confirming_selection(
                            &mut target,
                            &mut mode,
                            &mut cloud_security,
                            focus,
                            cloud_available,
                            false,
                        );
                    }
                    KeyCode::Right | KeyCode::Down => {
                        adjust_confirming_selection(
                            &mut target,
                            &mut mode,
                            &mut cloud_security,
                            focus,
                            cloud_available,
                            true,
                        );
                    }
                    KeyCode::Enter | KeyCode::Char('y') => {
                        self.begin_upload(target, mode, cloud_api_url.as_deref(), cloud_security);
                        return;
                    }
                    KeyCode::Esc | KeyCode::Char('n') => {
                        self.cancel_upload_operation();
                        return;
                    }
                    _ => {}
                }

                focus = normalize_confirming_focus(focus, cloud_available, target);
                self.upload_state = UploadState::Confirming {
                    target,
                    mode,
                    cloud_api_url,
                    cloud_security,
                    focus,
                };
            }
            UploadState::SecretConfirming { .. } => match key {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.approve_secrets_and_continue_upload();
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.cancel_upload_operation();
                }
                _ => {}
            },
            UploadState::SecretScanning
            | UploadState::Compressing { .. }
            | UploadState::Uploading { .. } => {
                if matches!(key, KeyCode::Esc | KeyCode::Char('n')) {
                    self.cancel_upload_operation();
                }
            }
            UploadState::Complete { .. } | UploadState::Error { .. } => {
                // Any key dismisses the result
                self.cancel_upload_operation();
            }
        }
    }

    fn handle_resume_key(&mut self, key: KeyCode) {
        match &self.resume_state {
            ResumeState::Confirming { command } => {
                match key {
                    KeyCode::Enter | KeyCode::Char('y') => {
                        // Copy command to clipboard
                        let cmd = command.clone();
                        let _ = share::copy_to_clipboard(&cmd);
                        self.resume_state = ResumeState::Complete { command: cmd };
                    }
                    KeyCode::Esc | KeyCode::Char('n') => {
                        self.resume_state = ResumeState::Idle;
                    }
                    _ => {}
                }
            }
            ResumeState::Complete { .. } => {
                // Any key dismisses the result
                self.resume_state = ResumeState::Idle;
            }
            ResumeState::Idle => {}
        }
    }

    fn handle_turn_metadata_key(&mut self, key: KeyCode) {
        if let TurnMetadataState::Showing { content, .. } = &self.turn_metadata_state {
            if key == KeyCode::Char('c') {
                self.copy_to_clipboard(Some(content.clone()), CopySource::Tab("Metadata".into()));
                return;
            }
        }
        self.turn_metadata_state = TurnMetadataState::Idle;
    }

    fn open_turn_metadata_view(&mut self) {
        match self.resolve_metadata_session() {
            Ok(session) => {
                self.turn_metadata_state = TurnMetadataState::Showing {
                    session_name: session.name.clone(),
                    content: build_turn_metadata_report(&session),
                };
            }
            Err(message) => {
                self.error_message = Some(message);
            }
        }
    }

    fn resolve_metadata_session(&self) -> std::result::Result<Session, String> {
        if let Some(session) = &self.session {
            return Ok(session.clone());
        }

        let selected = self
            .session_list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
            .ok_or_else(|| "No session selected".to_string())?;

        selected
            .parse()
            .map_err(|e| format!("Failed to parse session: {e}"))
    }

    fn begin_upload(
        &mut self,
        target: ShareTarget,
        mode: share::ShareExportMode,
        cloud_api_url: Option<&str>,
        cloud_security: CloudShareSecurity,
    ) {
        self.text_selection = None;
        self.upload_worker_rx = None;
        self.pending_cloud_upload = None;

        let (selected, session) = match self.resolve_upload_context() {
            Ok(ctx) => ctx,
            Err(message) => {
                self.upload_state = UploadState::Error { message };
                return;
            }
        };

        match target {
            ShareTarget::Cloud => {
                let Some(api_url) = cloud_api_url else {
                    self.upload_state = UploadState::Error {
                        message: "Cloud share endpoint is unavailable.".to_string(),
                    };
                    return;
                };

                let auth_token = match auth::load_auth_token() {
                    Some(token) => token,
                    None => {
                        self.upload_state = UploadState::Error {
                            message: format!(
                                "Cloud upload requires login. Run `vibereview login` or set {}.",
                                auth::GITHUB_TOKEN_ENV
                            ),
                        };
                        return;
                    }
                };

                self.pending_cloud_upload = Some(PendingCloudUpload {
                    session: session.clone(),
                    api_url: api_url.to_string(),
                    auth_token,
                    cloud_security,
                });
                self.upload_state = UploadState::SecretScanning;
                self.spawn_upload_worker(move |tx| {
                    let mut findings = share::scan_session_for_secrets(&session, 5);
                    if findings.is_empty() {
                        findings = share::scan_paths_for_secrets(&selected.paths, 5);
                    }
                    let _ = tx.send(UploadWorkerMessage::SecretScanFinished { findings });
                });
            }
            ShareTarget::File => {
                self.upload_state = UploadState::Compressing {
                    target: ShareTarget::File,
                };
                self.spawn_upload_worker(move |tx| {
                    let result = run_file_export_job(session, selected, mode, &tx);
                    let _ = tx.send(UploadWorkerMessage::FileExportFinished { result });
                });
            }
        }
    }

    fn resolve_upload_context(&self) -> std::result::Result<(UnifiedSession, Session), String> {
        let selected = self
            .selected_unified_session()
            .cloned()
            .or_else(|| {
                self.current_session_index
                    .and_then(|i| self.sessions.get(i).cloned())
            })
            .ok_or_else(|| "No session selected".to_string())?;

        let session = match &self.session {
            Some(s) => s.clone(),
            None => selected
                .parse()
                .map_err(|e| format!("Failed to parse session: {e}"))?,
        };

        Ok((selected, session))
    }

    fn approve_secrets_and_continue_upload(&mut self) {
        let Some(pending) = self.pending_cloud_upload.take() else {
            self.upload_state = UploadState::Error {
                message: "Cloud upload context expired. Retry sharing.".to_string(),
            };
            return;
        };
        self.start_cloud_upload_worker(pending);
    }

    fn start_cloud_upload_worker(&mut self, pending: PendingCloudUpload) {
        self.upload_state = UploadState::Compressing {
            target: ShareTarget::Cloud,
        };
        self.spawn_upload_worker(move |tx| {
            let result = run_cloud_upload_job(pending, &tx);
            let _ = tx.send(UploadWorkerMessage::CloudUploadFinished { result });
        });
    }

    fn spawn_upload_worker<F>(&mut self, job: F)
    where
        F: FnOnce(mpsc::Sender<UploadWorkerMessage>) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.upload_worker_rx = Some(rx);
        std::thread::spawn(move || job(tx));
    }

    fn cancel_upload_operation(&mut self) {
        self.upload_state = UploadState::Idle;
        self.text_selection = None;
        self.upload_worker_rx = None;
        self.pending_cloud_upload = None;
    }

    fn selected_unified_session(&self) -> Option<&UnifiedSession> {
        if let Some(i) = self.current_session_index {
            return self.sessions.get(i);
        }
        self.session_list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
    }

    fn handle_session_browser_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('u') => {
                // Share selected session
                if self.session_list_state.selected().is_some() {
                    let cloud_api_url = share::cloud_share_api_url();
                    let cloud_available = cloud_api_url.is_some();
                    let target = if cloud_available {
                        ShareTarget::Cloud
                    } else {
                        ShareTarget::File
                    };
                    self.upload_state = UploadState::Confirming {
                        target,
                        mode: share::ShareExportMode::FullSession,
                        cloud_api_url,
                        cloud_security: CloudShareSecurity::Encrypted,
                        focus: default_confirming_focus(cloud_available, target),
                    };
                }
            }
            KeyCode::Up => {
                Self::select_prev_in_list(&mut self.session_list_state, self.sessions.len())
            }
            KeyCode::Down => {
                Self::select_next_in_list(&mut self.session_list_state, self.sessions.len())
            }
            KeyCode::PageUp => {
                let len = self.sessions.len();
                for _ in 0..10 {
                    Self::select_prev_in_list(&mut self.session_list_state, len);
                }
            }
            KeyCode::PageDown => {
                let len = self.sessions.len();
                for _ in 0..10 {
                    Self::select_next_in_list(&mut self.session_list_state, len);
                }
            }
            KeyCode::Enter => {
                if let Some(i) = self.session_list_state.selected() {
                    if let Some(session_info) = self.sessions.get(i) {
                        match session_info.parse() {
                            Ok(session) => {
                                let context =
                                    TurnContext::new(session.name.clone(), session.turns.clone());
                                self.session = Some(session);
                                self.current_session_index = Some(i);
                                self.context_stack = vec![context];
                                self.view = View::SessionViewer;
                            }
                            Err(e) => {
                                self.error_message = Some(format!("Failed to parse session: {e}"));
                            }
                        }
                    }
                }
            }
            KeyCode::Char('R') => {
                // Show resume confirmation for selected session
                if let Some(i) = self.session_list_state.selected() {
                    if let Some(session_info) = self.sessions.get(i) {
                        let cmd = session_info.get_resume_command();
                        self.resume_state = ResumeState::Confirming { command: cmd };
                    }
                }
            }
            KeyCode::Char('/') => {
                self.start_search(SearchScope::SessionList);
            }
            KeyCode::Char('m') => {
                self.open_turn_metadata_view();
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
                    let cloud_api_url = share::cloud_share_api_url();
                    let cloud_available = cloud_api_url.is_some();
                    let target = if cloud_available {
                        ShareTarget::Cloud
                    } else {
                        ShareTarget::File
                    };
                    self.upload_state = UploadState::Confirming {
                        target,
                        mode: share::ShareExportMode::FullSession,
                        cloud_api_url,
                        cloud_security: CloudShareSecurity::Encrypted,
                        focus: default_confirming_focus(cloud_available, target),
                    };
                }
            }
            KeyCode::Esc => {
                // Pop subagent context or go back to session browser
                if self.context_stack.len() > 1 {
                    self.context_stack.pop();
                } else {
                    self.view = View::SessionBrowser;
                    self.session = None;
                    self.current_session_index = None;
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
                        let tool_count =
                            ctx.selected_turn().map_or(0, |t| t.tool_invocations.len());
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
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(ctx) = self.current_context_mut() {
                    ctx.scroll_offset = u16::MAX;
                }
            }
            KeyCode::Home => {
                if let Some(ctx) = self.current_context_mut() {
                    ctx.scroll_offset = 0;
                }
            }
            KeyCode::PageUp => {
                let page = self.content_area.map_or(10, |a| a.height.saturating_sub(1));
                if let Some(ctx) = self.current_context_mut() {
                    ctx.scroll_offset = ctx.scroll_offset.saturating_sub(page);
                }
            }
            KeyCode::PageDown => {
                let page = self.content_area.map_or(10, |a| a.height.saturating_sub(1));
                if let Some(ctx) = self.current_context_mut() {
                    ctx.scroll_offset = ctx.scroll_offset.saturating_add(page);
                }
            }
            KeyCode::Char('/') => {
                let scope = match self.current_context().map(|c| c.active_tab) {
                    Some(DetailTab::Diff) => SearchScope::Diff,
                    _ => SearchScope::Content,
                };
                self.start_search(scope);
            }
            KeyCode::Char('f') => {
                self.start_search(SearchScope::TurnList);
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
            KeyCode::Char('R') => {
                // Show resume confirmation
                if let Some(i) = self.current_session_index {
                    if let Some(session_info) = self.sessions.get(i) {
                        let cmd = session_info.get_resume_command();
                        self.resume_state = ResumeState::Confirming { command: cmd };
                    }
                }
            }
            KeyCode::Char('m') => {
                self.open_turn_metadata_view();
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
                if let ToolType::Task {
                    subagent_turns,
                    subagent_type,
                    description,
                    ..
                } = &tool.tool_type
                {
                    if !subagent_turns.is_empty() {
                        let title = subagent_type
                            .as_deref()
                            .unwrap_or(description.as_str())
                            .to_string();
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

    fn start_search(&mut self, scope: SearchScope) {
        let mut actual_scope = scope;
        if matches!(scope, SearchScope::Diff) {
            let has_diff = self.get_diff_text().is_some_and(|d| !d.is_empty());
            if !has_diff {
                actual_scope = SearchScope::Content;
            }
        }

        let lines = self.search_lines_for_scope(actual_scope);
        self.search = Some(SearchState {
            scope: actual_scope,
            query: String::new(),
            hits: Vec::new(),
            cursor: 0,
            lines,
            committed: false,
        });
    }

    fn search_lines_for_scope(&self, scope: SearchScope) -> Vec<String> {
        match scope {
            SearchScope::SessionList => self
                .sessions
                .iter()
                .map(|s| {
                    let desc = s.description.as_deref().unwrap_or(&s.name);
                    format!("{} {} {}", s.name, s.project, desc)
                })
                .collect(),
            SearchScope::TurnList => self
                .current_context()
                .map(|ctx| {
                    ctx.turns
                        .iter()
                        .enumerate()
                        .map(|(i, t)| format!("{} {}", i + 1, t.user_prompt))
                        .collect()
                })
                .unwrap_or_default(),
            SearchScope::Content => {
                if self.content_lines.is_empty() {
                    self.get_copyable_content()
                        .map(|text| text.lines().map(std::string::ToString::to_string).collect())
                        .unwrap_or_default()
                } else {
                    self.content_lines.clone()
                }
            }
            SearchScope::Diff => {
                if self.content_lines.is_empty() {
                    self.get_diff_text()
                        .map(|text| text.lines().map(std::string::ToString::to_string).collect())
                        .unwrap_or_default()
                } else {
                    self.content_lines.clone()
                }
            }
        }
    }

    fn update_search_hits(search: &mut SearchState) {
        search.hits.clear();
        search.cursor = 0;

        let query = search.query.trim().to_string();
        if query.is_empty() {
            return;
        }

        let q = query.to_lowercase();

        for (line_idx, line) in search.lines.iter().enumerate() {
            let lower = line.to_lowercase();
            let mut start = 0usize;
            while start <= lower.len() {
                if let Some(found) = lower[start..].find(&q) {
                    let byte_col = start + found;
                    let col = byte_to_char_idx(line, byte_col);
                    search.hits.push(SearchHit {
                        line: line_idx,
                        col,
                    });
                    start = byte_col + q.len().max(1);
                } else {
                    break;
                }
            }
        }
    }

    fn apply_search_hit(&mut self) {
        let (scope, hit_opt) = match &self.search {
            Some(search) if !search.hits.is_empty() => {
                let hit =
                    search.hits[search.cursor.min(search.hits.len().saturating_sub(1))].clone();
                (search.scope, Some(hit))
            }
            _ => (SearchScope::Content, None),
        };

        let Some(hit) = hit_opt else {
            return;
        };

        match scope {
            SearchScope::SessionList => {
                if hit.line < self.sessions.len() {
                    self.session_list_state.select(Some(hit.line));
                }
            }
            SearchScope::TurnList => {
                if let Some(ctx) = self.current_context_mut() {
                    if hit.line < ctx.turns.len() {
                        ctx.turn_list_state.select(Some(hit.line));
                        ctx.scroll_offset = 0;
                        ctx.tool_scroll_offset = 0;
                    }
                }
            }
            SearchScope::Content | SearchScope::Diff => {
                if let Some(ctx) = self.current_context_mut() {
                    ctx.scroll_offset = hit.line as u16;
                }
            }
        }
    }

    fn search_next(&mut self) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        if search.hits.is_empty() {
            return;
        }
        search.cursor = (search.cursor + 1) % search.hits.len();
        self.apply_search_hit();
    }

    fn search_prev(&mut self) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        if search.hits.is_empty() {
            return;
        }
        if search.cursor == 0 {
            search.cursor = search.hits.len().saturating_sub(1);
        } else {
            search.cursor -= 1;
        }
        self.apply_search_hit();
    }

    fn handle_search_key(&mut self, key: KeyCode) {
        let Some(search) = self.search.as_mut() else {
            return;
        };

        match key {
            KeyCode::Esc => {
                self.search = None;
            }
            KeyCode::Enter => {
                self.apply_search_hit();
                if let Some(search) = self.search.as_mut() {
                    search.committed = true;
                }
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.committed = false;
                Self::update_search_hits(search);
            }
            KeyCode::Char('n') => {
                if search.committed {
                    self.search_next();
                } else {
                    search.query.push('n');
                    Self::update_search_hits(search);
                }
            }
            KeyCode::Char('p') => {
                if search.committed {
                    self.search_prev();
                } else {
                    search.query.push('p');
                    Self::update_search_hits(search);
                }
            }
            KeyCode::Char(c) => {
                if !c.is_control() {
                    search.query.push(c);
                    search.committed = false;
                    Self::update_search_hits(search);
                }
            }
            _ => {}
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
            DetailTab::ToolCalls => ctx.selected_tool().map(|tool| {
                let tool_kind = if is_subagent_tool(tool) {
                    "subagent"
                } else if is_planning_tool(tool) {
                    "planning"
                } else {
                    "regular"
                };
                format!(
                    "Tool: {} ({})\n\nInput:\n{}\n\nOutput:\n{}",
                    tool.tool_type.name(),
                    tool_kind,
                    tool.input_display,
                    tool.output_display
                )
            }),
            DetailTab::Diff => {
                let mut diffs = String::new();
                for tool in &turn.tool_invocations {
                    if let Some(diff) = tool.tool_type.diff() {
                        let path = match &tool.tool_type {
                            ToolType::FileEdit { path, .. } | ToolType::FileWrite { path, .. } => {
                                path.clone()
                            }
                            _ => "unknown".to_string(),
                        };
                        let _ = writeln!(diffs, "--- {path} ---\n{diff}\n");
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

    fn get_diff_text(&self) -> Option<String> {
        let ctx = self.current_context()?;
        let turn = ctx.selected_turn()?;
        let mut diffs = String::new();
        for tool in &turn.tool_invocations {
            if let Some(diff) = tool.tool_type.diff() {
                let path = match &tool.tool_type {
                    ToolType::FileEdit { path, .. } | ToolType::FileWrite { path, .. } => {
                        path.clone()
                    }
                    _ => "unknown".to_string(),
                };
                let _ = writeln!(diffs, "--- {path} ---\n{diff}\n");
            }
            if let ToolType::Task {
                subagent_turns,
                subagent_type,
                ..
            } = &tool.tool_type
            {
                if !subagent_turns.is_empty() {
                    let prefix = format!("[{}]", subagent_type.as_deref().unwrap_or("subagent"));
                    for subturn in subagent_turns {
                        for subtool in &subturn.tool_invocations {
                            if let Some(subdiff) = subtool.tool_type.diff() {
                                let path = match &subtool.tool_type {
                                    ToolType::FileEdit { path, .. }
                                    | ToolType::FileWrite { path, .. } => path.clone(),
                                    _ => "unknown".to_string(),
                                };
                                let _ = writeln!(diffs, "--- {prefix} {path} ---\n{subdiff}\n");
                            }
                        }
                    }
                }
            }
        }
        if diffs.is_empty() {
            None
        } else {
            Some(diffs)
        }
    }

    /// Get the current tab name
    fn current_tab_name(&self) -> String {
        self.current_context()
            .map_or("content", |ctx| match ctx.active_tab {
                DetailTab::Prompt => "Prompt",
                DetailTab::Thinking => "Thinking",
                DetailTab::ToolCalls => "Tool Calls",
                DetailTab::Diff => "Diff",
            })
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
        let modal_selectable = matches!(self.upload_state, UploadState::Complete { .. });
        if !modal_selectable && self.view != View::SessionViewer {
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
                    let rel_x = x
                        .saturating_sub(content_area.x)
                        .min(content_area.width.saturating_sub(1));
                    let rel_y = y
                        .saturating_sub(content_area.y)
                        .min(content_area.height.saturating_sub(1));
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
                self.text_selection = None;
            }
            MouseEventKind::ScrollUp => {
                if in_content {
                    if let Some(ctx) = self.current_context_mut() {
                        ctx.scroll_offset = ctx.scroll_offset.saturating_sub(3);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if in_content {
                    if let Some(ctx) = self.current_context_mut() {
                        ctx.scroll_offset = ctx.scroll_offset.saturating_add(3);
                    }
                }
            }
            _ => {}
        }
    }

    /// Extract selected text from `content_lines` based on current selection
    fn extract_selected_text(&self) -> Option<String> {
        let selection = self.text_selection.as_ref()?;

        if self.content_lines.is_empty() {
            return None;
        }

        let ((start_row, start_col), (end_row, end_col)) = selection.normalized();
        let scroll = self.selection_scroll_offset as usize;

        let mut result = String::new();

        for (i, rel_row) in (start_row..=end_row).enumerate() {
            let line_idx = scroll + rel_row as usize;
            if line_idx >= self.content_lines.len() {
                break;
            }

            let line = &self.content_lines[line_idx];
            let chars: Vec<char> = line.chars().collect();

            let line_start = if rel_row == start_row {
                start_col as usize
            } else {
                0
            };
            let line_end = if rel_row == end_row {
                (end_col as usize + 1).min(chars.len())
            } else {
                chars.len()
            };

            if line_start < chars.len() {
                let selected: String = chars[line_start..line_end.min(chars.len())]
                    .iter()
                    .collect();
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

    // Render resume modal if active
    if !matches!(app.resume_state, ResumeState::Idle) {
        render_resume_modal(frame, app);
    }

    // Render turn metadata debug modal if active
    if !matches!(app.turn_metadata_state, TurnMetadataState::Idle) {
        render_turn_metadata_modal(frame, app);
    }
}

fn render_upload_modal(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Create centered modal area
    let modal_width = 72.min(area.width.saturating_sub(4));
    let modal_height = 22.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width - modal_width) / 2,
        y: (area.height - modal_height) / 2,
        width: modal_width,
        height: modal_height,
    };

    // Clear background
    frame.render_widget(Clear, modal_area);

    let (title, content, style, selectable) = match &app.upload_state {
        UploadState::Idle => return,
        UploadState::Confirming {
            target,
            mode,
            cloud_api_url: _cloud_api_url,
            cloud_security,
            focus,
        } => {
            let session_name = app
                .session
                .as_ref()
                .map(|s| s.name.clone())
                .or_else(|| {
                    app.session_list_state
                        .selected()
                        .and_then(|i| app.sessions.get(i))
                        .map(|s| s.name.clone())
                })
                .unwrap_or_else(|| "Unknown".to_string());
            let section_marker = |section: ShareDialogSection| {
                if *focus == section {
                    ">"
                } else {
                    " "
                }
            };
            let radio = |selected: bool| if selected { "[x]" } else { "[ ]" };
            let content = if *target == ShareTarget::Cloud {
                let key_note = match cloud_security {
                    CloudShareSecurity::Encrypted => format!(
                        "Encrypted link uploads ciphertext.\n\
                        Key is appended as '#{}=...'.\n\
                        Share full URL for access.",
                        share::SHARE_KEY_PARAM
                    ),
                    CloudShareSecurity::Public => "Public link uploads readable payload.\n\
                        No key required to view/import."
                        .to_string(),
                };
                format!(
                    "Share \"{}\".\n\n\
                    {} Destination:\n\
                      {} Cloud link\n\
                      {} File export\n\n\
                    {} Security:\n\
                      {} Encrypted (recommended)\n\
                      {} Public (not encrypted)\n\n\
                    {}\n\n\
                    Tab/Shift+Tab: next/prev section\n\
                    Arrows: change selected radio\n\
                    Enter or 'y': share | Esc or 'n': cancel",
                    truncate_str(&session_name, 36),
                    section_marker(ShareDialogSection::Destination),
                    radio(*target == ShareTarget::Cloud),
                    radio(*target == ShareTarget::File),
                    section_marker(ShareDialogSection::CloudSecurity),
                    radio(*cloud_security == CloudShareSecurity::Encrypted),
                    radio(*cloud_security == CloudShareSecurity::Public),
                    key_note,
                )
            } else {
                let resumable_hint = if mode.is_resumable() {
                    "Yes (includes resume artifacts)"
                } else {
                    "No (redacted export)"
                };
                format!(
                    "Share \"{}\".\n\n\
                    {} Destination:\n\
                      {} Cloud link\n\
                      {} File export\n\n\
                    {} Export mode:\n\
                      {} Prompt + response only\n\
                      {} Prompt + response + diff\n\
                      {} Full session (resumable)\n\n\
                    Resumable on another machine: {}\n\n\
                    Tab/Shift+Tab: next/prev section\n\
                    Arrows: change selected radio\n\
                    Enter or 'y': export | Esc or 'n': cancel",
                    truncate_str(&session_name, 36),
                    section_marker(ShareDialogSection::Destination),
                    radio(*target == ShareTarget::Cloud),
                    radio(*target == ShareTarget::File),
                    section_marker(ShareDialogSection::ExportMode),
                    radio(*mode == share::ShareExportMode::PromptResponseOnly),
                    radio(*mode == share::ShareExportMode::PromptResponseAndDiff),
                    radio(*mode == share::ShareExportMode::FullSession),
                    resumable_hint
                )
            };
            (
                " Share Session ",
                content,
                Style::default().fg(Color::Yellow),
                false,
            )
        }
        UploadState::SecretScanning => (
            " Secret Scan ",
            "Scanning session for potential secrets...\n\n\
            This runs locally before any cloud upload.\n\n\
            Press Esc or 'n' to cancel."
                .to_string(),
            Style::default().fg(Color::Cyan),
            false,
        ),
        UploadState::SecretConfirming { findings } => (
            " Secrets Detected ",
            format_secret_scan_findings(findings),
            Style::default().fg(Color::Yellow),
            false,
        ),
        UploadState::Compressing { target } => match target {
            ShareTarget::Cloud => (
                " Sharing... ",
                "Preparing cloud payload...".to_string(),
                Style::default().fg(Color::Cyan),
                false,
            ),
            ShareTarget::File => (
                " Exporting... ",
                "Building share file...".to_string(),
                Style::default().fg(Color::Cyan),
                false,
            ),
        },
        UploadState::Uploading { target } => match target {
            ShareTarget::Cloud => (
                " Sharing... ",
                "Uploading cloud payload...".to_string(),
                Style::default().fg(Color::Cyan),
                false,
            ),
            ShareTarget::File => (
                " Exporting... ",
                "Saving file...".to_string(),
                Style::default().fg(Color::Cyan),
                false,
            ),
        },
        UploadState::Complete {
            target,
            location,
            resumable,
            cloud_security,
        } => match target {
            ShareTarget::Cloud => {
                let security = cloud_security.unwrap_or(CloudShareSecurity::Encrypted);
                let security_text = match security {
                    CloudShareSecurity::Encrypted => format!(
                        "Security: {}\n\
                        Includes decryption key in '#{}=...'.\n\
                        Keep the full URL when sharing.",
                        security.label(),
                        share::SHARE_KEY_PARAM
                    ),
                    CloudShareSecurity::Public => {
                        format!(
                            "Security: {}\nNo decryption key required.",
                            security.label()
                        )
                    }
                };
                (
                    " Share Complete ",
                    format!(
                        "Session shared successfully!\n\n\
                        URL: {location}\n\n\
                        {}\n\
                        (Copied to clipboard)\n\n\
                        To open locally via CLI:\n\
                        {}\n\n\
                        Press any key to close",
                        security_text,
                        share_import_command(location),
                    ),
                    Style::default().fg(Color::Green),
                    true,
                )
            }
            ShareTarget::File => (
                " Export Complete ",
                format!(
                    "Session exported successfully!\n\n\
                    File: {location}\n\n\
                    Resumable: {}\n\
                    (Path copied to clipboard)\n\n\
                    To open this file on another machine:\n\
                    {}\n\n\
                    Press any key to close",
                    if *resumable { "yes" } else { "no" },
                    share_import_command(location),
                ),
                Style::default().fg(Color::Green),
                true,
            ),
        },
        UploadState::Error { message } => (
            " Share Failed ",
            format!(
                "Error: {message}\n\n\
                Press any key to close"
            ),
            Style::default().fg(Color::Red),
            false,
        ),
    };

    let inner_area = Rect {
        x: modal_area.x + 1,
        y: modal_area.y + 1,
        width: modal_area.width.saturating_sub(2),
        height: modal_area.height.saturating_sub(2),
    };

    let content_lines: Vec<String> = if selectable {
        wrap_text_for_selection(&content, inner_area.width)
    } else {
        content
            .lines()
            .map(std::string::ToString::to_string)
            .collect()
    };

    let render_text = if selectable {
        content_lines.join("\n")
    } else {
        content.clone()
    };

    let text = if selectable {
        apply_selection_highlight(
            Text::from(render_text.clone()),
            app.text_selection.as_ref(),
            0,
            inner_area.width,
        )
    } else {
        Text::from(render_text)
    };

    let mut modal = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(style)
                .title(title)
                .title_style(style.add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().fg(Color::White));
    if !selectable {
        modal = modal.wrap(Wrap { trim: false });
    }

    frame.render_widget(modal, modal_area);

    if selectable {
        app.content_area = Some(inner_area);
        app.content_lines = content_lines;
        app.selection_scroll_offset = 0;
    } else {
        app.content_area = None;
        app.content_lines.clear();
        app.selection_scroll_offset = 0;
    }
}

fn wrap_text_for_selection(text: &str, width: u16) -> Vec<String> {
    let max_cols = usize::from(width.max(1));
    let mut wrapped = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            wrapped.push(String::new());
            continue;
        }

        let mut chunk = String::new();
        let mut col = 0_usize;
        for ch in line.chars() {
            chunk.push(ch);
            col += 1;
            if col >= max_cols {
                wrapped.push(std::mem::take(&mut chunk));
                col = 0;
            }
        }

        if !chunk.is_empty() {
            wrapped.push(chunk);
        }
    }

    if text.ends_with('\n') {
        wrapped.push(String::new());
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }

    wrapped
}

fn text_to_plain_string(content: &Text<'_>) -> String {
    content
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_resume_modal(frame: &mut Frame, app: &App) {
    if matches!(app.resume_state, ResumeState::Idle) {
        return;
    }

    let area = frame.area();
    let modal_width = 70.min(area.width.saturating_sub(4));
    let modal_height = 12.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width - modal_width) / 2,
        y: (area.height - modal_height) / 2,
        width: modal_width,
        height: modal_height,
    };

    // Clear background
    frame.render_widget(Clear, modal_area);

    let (title, content, style) = match &app.resume_state {
        ResumeState::Idle => return,
        ResumeState::Confirming { command } => (
            " Resume Session ",
            format!(
                "Resume this session?\n\n\
                Command:\n\
                {command}\n\n\
                Press Enter or 'y' to copy command, Esc or 'n' to cancel"
            ),
            Style::default().fg(Color::Yellow),
        ),
        ResumeState::Complete { command } => (
            " Resume Command Copied ",
            format!(
                "Command copied to clipboard!\n\n\
                {command}\n\n\
                Paste and run in your terminal.\n\n\
                Press any key to close"
            ),
            Style::default().fg(Color::Green),
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

fn render_turn_metadata_modal(frame: &mut Frame, app: &App) {
    let TurnMetadataState::Showing {
        session_name,
        content,
    } = &app.turn_metadata_state
    else {
        return;
    };

    let area = frame.area();
    let modal_width = 92.min(area.width.saturating_sub(4));
    let modal_height = 24.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width - modal_width) / 2,
        y: (area.height - modal_height) / 2,
        width: modal_width,
        height: modal_height,
    };

    frame.render_widget(Clear, modal_area);

    let modal = Paragraph::new(format!(
        "Session: {}\n\n{}\n\nPress c to copy, any other key to close",
        truncate_str(session_name, 64),
        content
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Turn Metadata ")
            .title_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
    )
    .wrap(Wrap { trim: false })
    .style(Style::default().fg(Color::White));

    frame.render_widget(modal, modal_area);
}

fn render_session_browser(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Column widths:
    // ID: 15 chars (e.g., "063cd168abcd×2")
    // TIME: 8 chars (e.g., "12h ago")
    // SOURCE: 6 chars (e.g., "claude")
    // PROJECT: 16 chars
    // DESCRIPTION: remaining
    let id_width = 15;
    let time_width = 8;
    let source_width = 6;
    let project_width = 16;
    // borders(2) + highlight(2) + spacing(4 separators * 2 = 8) = 12
    let desc_width = (area.width as usize)
        .saturating_sub(12 + id_width + time_width + source_width + project_width)
        .max(10);

    let can_resume = app
        .session_list_state
        .selected()
        .and_then(|i| app.sessions.get(i))
        .is_some();
    let title = browser_title(app.sessions.len(), can_resume);

    // Render header line and list
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let header = format!(
        "  {:<id_w$}  {:<time_w$}  {:<src_w$}  {:<proj_w$}  {}",
        "ID",
        "TIME",
        "SOURCE",
        "PROJECT",
        "DESCRIPTION",
        id_w = id_width,
        time_w = time_width,
        src_w = source_width,
        proj_w = project_width
    );
    let header_para = Paragraph::new(header)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .title(title),
        );
    frame.render_widget(header_para, chunks[0]);

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            // Show part count for grouped sessions (e.g., "063cd168×2")
            let id = s.list_id();
            let time = format_time_ago(s.modified);
            let source = match s.source {
                Source::Claude => "claude",
                Source::Codex => "codex",
            };
            let project = truncate_str(&s.project, project_width);
            let desc = s.description.as_deref().unwrap_or(&s.name);
            let desc_display = truncate_str(desc, desc_width);

            let display = format!(
                "{id:<id_width$}  {time:<time_width$}  {source:<source_width$}  {project:<project_width$}  {desc_display}"
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

    let (help_text, help_style) = if let Some(search) = &app.search {
        let status = search_status_line(search);
        (status, Style::default().fg(Color::Yellow))
    } else {
        (
            browser_help_text(can_resume),
            Style::default().fg(Color::DarkGray),
        )
    };

    let help = Paragraph::new(help_text).style(help_style);
    frame.render_widget(help, chunks[2]);
}

fn format_time_ago(modified: Option<std::time::SystemTime>) -> String {
    modified
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            let secs = d.as_secs();
            let hours_ago = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|n| n.as_secs())
                .unwrap_or(secs)
                - secs)
                / 3600;
            if hours_ago < 24 {
                format!("{hours_ago}h ago")
            } else {
                format!("{}d ago", hours_ago / 24)
            }
        })
        .unwrap_or_default()
}

fn browser_title(session_count: usize, can_resume: bool) -> String {
    let resume = if can_resume { " | R: resume" } else { "" };
    format!(" Sessions ({session_count}) - Enter: open{resume} | m: metadata | u: share | q: quit ")
}

fn browser_help_text(can_resume: bool) -> String {
    let resume = if can_resume { " | R: Resume" } else { "" };
    format!(
        " ↑/↓: Navigate | PageUp/PageDown: Fast | Enter: Open | /: Search{resume} | m: Metadata | u: Share | q: Quit "
    )
}

fn viewer_help_text(is_subagent_view: bool, can_resume: bool) -> String {
    let resume = if can_resume { " | R: Resume" } else { "" };
    if is_subagent_view {
        format!(
            " ↑/↓: Navigate | ←/→: Tabs | j/k: Scroll | PageUp/PageDown: Fast | /: Search | f: Find Turn | c: Copy | m: Metadata{resume} | Esc: Back | q: Quit "
        )
    } else {
        format!(
            " ↑/↓: Navigate | ←/→: Tabs | j/k: Scroll | PageUp/PageDown: Fast | /: Search | f: Find Turn | c: Copy | m: Metadata{resume} | Enter: Open | q: Quit "
        )
    }
}

fn build_turn_metadata_report(session: &Session) -> String {
    if session.turns.is_empty() {
        return "No turns in this session.".to_string();
    }

    let mut out = format!("Turns: {}\n\n", session.turns.len());
    for (idx, turn) in session.turns.iter().enumerate() {
        let model = turn.model.as_deref().unwrap_or("-");
        let effort = turn.thinking_effort.as_deref().unwrap_or("-");
        let prompt = truncate_str(&turn.user_prompt, 56);
        let _ = writeln!(
            out,
            "{:>3}. model={} | thinking_effort={} | prompt={}",
            idx + 1,
            model,
            effort,
            prompt
        );
    }
    out
}

fn format_secret_scan_findings(findings: &[share::SecretScanFinding]) -> String {
    const DISPLAY_LIMIT: usize = 3;
    let mut lines = vec![
        "Potential secrets detected in this session.".to_string(),
        String::new(),
    ];

    for finding in findings.iter().take(DISPLAY_LIMIT) {
        lines.push(format!("- {}", finding.summary()));
    }
    if findings.len() > DISPLAY_LIMIT {
        lines.push(format!("- ... and {} more", findings.len() - DISPLAY_LIMIT));
    }

    lines.push(String::new());
    lines.push("Upload anyway?".to_string());
    lines.push("Enter or 'y': continue upload".to_string());
    lines.push("Esc or 'n': cancel".to_string());
    lines.join("\n")
}

fn run_file_export_job(
    session: Session,
    selected: UnifiedSession,
    mode: share::ShareExportMode,
    tx: &mpsc::Sender<UploadWorkerMessage>,
) -> std::result::Result<FileExportResult, String> {
    let _ = tx.send(UploadWorkerMessage::Progress(
        UploadWorkerProgress::Compressing(ShareTarget::File),
    ));

    let resume_input = if mode.is_resumable() {
        Some(share::ResumeBundleInput {
            source: match selected.source {
                Source::Claude => share::ResumeSource::Claude,
                Source::Codex => share::ResumeSource::Codex,
            },
            resume_session_id: selected.resume_session_id(),
            resume_command: selected.get_resume_command(),
            project_path_hint: selected.project_path.clone(),
            session_paths: selected.paths.clone(),
        })
    } else {
        None
    };

    let payload = share::build_share_file(&session, mode, resume_input.as_ref())
        .map_err(|e| format!("Export failed: {e}"))?;

    let path = share::default_share_file_path(&selected.name, mode);
    let _ = tx.send(UploadWorkerMessage::Progress(
        UploadWorkerProgress::Uploading(ShareTarget::File),
    ));
    share::write_share_file(&path, &payload).map_err(|e| format!("Save failed: {e}"))?;

    Ok(FileExportResult {
        location: path.display().to_string(),
        resumable: mode.is_resumable(),
    })
}

fn run_cloud_upload_job(
    pending: PendingCloudUpload,
    tx: &mpsc::Sender<UploadWorkerMessage>,
) -> std::result::Result<CloudUploadResult, String> {
    let _ = tx.send(UploadWorkerMessage::Progress(
        UploadWorkerProgress::Compressing(ShareTarget::Cloud),
    ));
    let compressed = share::compress_session(&pending.session)
        .map_err(|e| format!("Compression failed: {e}"))?;

    let fingerprint = share::session_fingerprint(&pending.session)
        .map_err(|e| format!("Failed to fingerprint session: {e}"))?;

    let security = match pending.cloud_security {
        CloudShareSecurity::Encrypted => "encrypted",
        CloudShareSecurity::Public => "public",
    };

    let mut share_key = None;
    let payload = match pending.cloud_security {
        CloudShareSecurity::Encrypted => {
            let key = share::generate_cloud_share_key();
            let payload = share::encrypt_cloud_payload(&compressed, &key)
                .map_err(|e| format!("Encryption failed: {e}"))?;
            share_key = Some(key);
            payload
        }
        CloudShareSecurity::Public => compressed,
    };

    let _ = tx.send(UploadWorkerMessage::Progress(
        UploadWorkerProgress::Uploading(ShareTarget::Cloud),
    ));
    let response = share::upload_session(
        &payload,
        &pending.api_url,
        &pending.auth_token,
        &fingerprint,
        &pending.session.name,
        pending.session.turns.len(),
        security,
    )
    .map_err(|e| format!("Upload failed: {e}"))?;

    let base_url = share::normalize_share_url(&response.url);
    let location = match share_key {
        Some(key) => share::attach_key_to_share_url(&base_url, &key),
        None => base_url,
    };

    Ok(CloudUploadResult {
        location,
        cloud_security: pending.cloud_security,
        reused: response.reused,
    })
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
            let prompt_preview: String = turn
                .user_prompt
                .chars()
                .take(40)
                .collect::<String>()
                .replace('\n', " ");

            let tool_count = turn.tool_invocations.len();
            let tool_info = if tool_count > 0 {
                format!(" [{tool_count}]")
            } else {
                String::new()
            };

            ListItem::new(format!("{}: {}{}", i + 1, prompt_preview, tool_info))
        })
        .collect();

    let title = if is_subagent {
        format!(
            " {} ({} turns) - Esc to go back ",
            ctx.title,
            ctx.turns.len()
        )
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
    let can_resume = app
        .current_session_index
        .and_then(|i| app.sessions.get(i))
        .is_some();
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

    let tab_area = if app.is_subagent_view() {
        chunks[1]
    } else {
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
        let scroll_offset = ctx.scroll_offset;
        let content: Text = match ctx.active_tab {
            DetailTab::Prompt => render_prompt_tab(turn),
            DetailTab::Thinking => render_thinking_tab(turn),
            DetailTab::ToolCalls => render_tool_calls_tab(turn, ctx.tool_scroll_offset),
            DetailTab::Diff => render_diff_tab(turn),
        };

        let content = apply_search_highlight(content, app.search.as_ref(), ctx.active_tab);
        let content_plain = text_to_plain_string(&content);
        let wrapped_lines = wrap_text_for_selection(&content_plain, inner_content_area.width);
        let selection_active = app.text_selection.is_some();

        let render_content = if selection_active {
            Text::from(wrapped_lines.join("\n"))
        } else {
            content
        };

        let render_content = apply_selection_highlight(
            render_content,
            app.text_selection.as_ref(),
            ctx.scroll_offset,
            inner_content_area.width,
        );

        let mut paragraph = Paragraph::new(render_content)
            .block(content_block)
            .scroll((ctx.scroll_offset, 0));
        if !selection_active {
            paragraph = paragraph.wrap(Wrap { trim: false });
        }
        frame.render_widget(paragraph, content_area);

        // Store for mouse handling (after we're done with ctx borrow)
        app.content_area = Some(inner_content_area);
        app.content_lines = wrapped_lines;
        app.selection_scroll_offset = scroll_offset;
    } else {
        let paragraph = Paragraph::new("Select a turn to view details")
            .block(content_block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, content_area);
        app.content_area = None;
        app.content_lines.clear();
        app.selection_scroll_offset = 0;
    }

    // Help line - show "Copied!" feedback briefly, otherwise show help
    let copy_feedback = app
        .copy_feedback
        .as_ref()
        .filter(|f| f.timestamp.elapsed() < COPY_FEEDBACK_DURATION);

    let (help_text, help_style) = if let Some(feedback) = copy_feedback {
        (
            format!(" ✓ Copied {} to clipboard! ", feedback.source),
            Style::default().fg(Color::Green),
        )
    } else if let Some(search) = &app.search {
        (
            search_status_line(search),
            Style::default().fg(Color::Yellow),
        )
    } else if app.is_subagent_view() {
        (
            viewer_help_text(true, can_resume),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            viewer_help_text(false, can_resume),
            Style::default().fg(Color::DarkGray),
        )
    };

    let help = Paragraph::new(help_text).style(help_style);
    frame.render_widget(help, help_area);
}

/// Apply selection highlighting to text content
fn apply_selection_highlight(
    content: Text<'static>,
    selection: Option<&TextSelection>,
    scroll_offset: u16,
    _width: u16,
) -> Text<'static> {
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
            let line_sel_start = if line_idx == sel_start_line {
                start_col as usize
            } else {
                0
            };
            let line_sel_end = if line_idx == sel_end_line {
                end_col as usize + 1
            } else {
                usize::MAX
            };

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
                    let selected: String =
                        chars[sel_start_in_span..sel_end_in_span].iter().collect();
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

fn apply_search_highlight(
    content: Text<'static>,
    search: Option<&SearchState>,
    active_tab: DetailTab,
) -> Text<'static> {
    let Some(search) = search else {
        return content;
    };

    let is_tab_match = match (search.scope, active_tab) {
        (SearchScope::Content, DetailTab::Diff) => false,
        (SearchScope::Diff, DetailTab::Diff) | (SearchScope::Content, _) => true,
        _ => false,
    };

    if !is_tab_match {
        return content;
    }

    let query = search.query.trim();
    if query.is_empty() {
        return content;
    }

    let current_hit = search.hits.get(search.cursor).map(|h| (h.line, h.col));

    let mut new_lines: Vec<Line<'static>> = Vec::new();
    for (line_idx, line) in content.lines.into_iter().enumerate() {
        let ranges = build_search_ranges(
            &line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            query,
            current_hit.filter(|(l, _)| *l == line_idx),
        );
        if ranges.is_empty() {
            new_lines.push(line);
            continue;
        }
        new_lines.push(apply_ranges_to_line(line, &ranges));
    }

    Text::from(new_lines)
}

fn build_search_ranges(
    line: &str,
    query: &str,
    current: Option<(usize, usize)>,
) -> Vec<(usize, usize, bool)> {
    let mut ranges = Vec::new();
    let line_lower = line.to_lowercase();
    let q_lower = query.to_lowercase();

    let mut start = 0usize;
    while start <= line_lower.len() {
        if let Some(found) = line_lower[start..].find(&q_lower) {
            let byte_col = start + found;
            let col = byte_to_char_idx(line, byte_col);
            let byte_end = byte_col + q_lower.len();
            let end = byte_to_char_idx(line, byte_end);
            let is_current = current.is_some_and(|(_, c)| c == col);
            ranges.push((col, end, is_current));
            start = byte_end.max(byte_col + 1);
        } else {
            break;
        }
    }

    ranges
}

fn apply_ranges_to_line(line: Line<'static>, ranges: &[(usize, usize, bool)]) -> Line<'static> {
    let highlight_style = Style::default().bg(SEARCH_HIGHLIGHT_BG);
    let current_style = Style::default().bg(SEARCH_CURRENT_BG).fg(SEARCH_CURRENT_FG);

    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    let mut range_idx = 0usize;

    for span in line.spans {
        let span_text = span.content.to_string();
        let span_len = span_text.chars().count();
        let span_start = cursor;
        let span_end = cursor + span_len;

        let chars: Vec<char> = span_text.chars().collect();
        let mut local_pos = 0usize;

        while range_idx < ranges.len() && ranges[range_idx].1 <= span_start {
            range_idx += 1;
        }

        let mut local_range_idx = range_idx;
        while local_range_idx < ranges.len() && ranges[local_range_idx].0 < span_end {
            let (range_start, range_end, is_current) = ranges[local_range_idx];
            let range_start_in_span = range_start.saturating_sub(span_start).min(span_len);
            let range_end_in_span = range_end.saturating_sub(span_start).min(span_len);

            if local_pos < range_start_in_span {
                let before: String = chars[local_pos..range_start_in_span].iter().collect();
                if !before.is_empty() {
                    new_spans.push(Span::styled(before, span.style));
                }
            }

            if range_start_in_span < range_end_in_span {
                let matched: String = chars[range_start_in_span..range_end_in_span]
                    .iter()
                    .collect();
                let style = if is_current {
                    span.style.patch(current_style)
                } else {
                    span.style.patch(highlight_style)
                };
                if !matched.is_empty() {
                    new_spans.push(Span::styled(matched, style));
                }
            }

            local_pos = range_end_in_span;
            local_range_idx += 1;
        }

        if local_pos < span_len {
            let after: String = chars[local_pos..span_len].iter().collect();
            if !after.is_empty() {
                new_spans.push(Span::styled(after, span.style));
            }
        }

        cursor = span_end;
    }

    Line::from(new_spans)
}

fn byte_to_char_idx(s: &str, byte_idx: usize) -> usize {
    s.get(..byte_idx).map_or(0, |prefix| prefix.chars().count())
}

fn search_status_line(search: &SearchState) -> String {
    let count = search.hits.len();
    if search.query.is_empty() {
        format!(
            " / Search {}: (type to search, Esc to close) ",
            search.scope
        )
    } else if count == 0 {
        format!(" / Search {}: {} (0 matches) ", search.scope, search.query)
    } else if search.committed {
        format!(
            " / Search {}: {} ({}/{})  n/p: next/prev  Enter: jump  Esc: close ",
            search.scope,
            search.query,
            search.cursor + 1,
            count
        )
    } else {
        format!(
            " / Search {}: {} ({}/{})  Enter: jump/enable n/p  Esc: close ",
            search.scope,
            search.query,
            search.cursor + 1,
            count
        )
    }
}

/// Truncate a string to `max_chars`, adding "…" if truncated
fn truncate_str(s: &str, max_chars: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > max_chars {
        format!(
            "{}…",
            s.chars()
                .take(max_chars.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        s
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
    while lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
    }

    lines
}

/// Compute style based on current state
#[allow(clippy::fn_params_excessive_bools)]
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
        Line::styled(
            "User Prompt:".to_string(),
            Style::default().fg(Color::Cyan).bold(),
        ),
        Line::from(""),
    ];

    // User prompt as plain text (don't interpret as markdown)
    for line in turn.user_prompt.lines() {
        lines.push(Line::from(line.to_string()));
    }

    if !turn.response.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "─".repeat(40),
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "Response:".to_string(),
            Style::default().fg(Color::Green).bold(),
        ));
        lines.push(Line::from(""));
        // Render response as markdown
        lines.extend(render_markdown(&turn.response));
    }

    Text::from(lines)
}

fn render_thinking_tab(turn: &Turn) -> Text<'static> {
    if let Some(thinking) = &turn.thinking {
        let heading = if let Some(effort) = &turn.thinking_effort {
            format!("Model Thinking ({effort}):")
        } else {
            "Model Thinking:".to_string()
        };
        let mut lines = vec![
            Line::styled(heading, Style::default().fg(Color::Magenta).bold()),
            Line::from(""),
        ];
        // Render thinking as markdown
        lines.extend(render_markdown(thinking));
        Text::from(lines)
    } else {
        Text::styled(
            "No thinking available for this turn".to_string(),
            Style::default().fg(Color::DarkGray),
        )
    }
}

fn render_tool_calls_tab(turn: &Turn, scroll_offset: usize) -> Text<'static> {
    if turn.tool_invocations.is_empty() {
        return Text::styled(
            "No tool calls in this turn".to_string(),
            Style::default().fg(Color::DarkGray),
        );
    }

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::styled(
        format!(
            "Tool Calls ({} total) - j/k to navigate, Enter to open subagent",
            turn.tool_invocations.len()
        ),
        Style::default().fg(Color::Cyan).bold(),
    ));
    lines.push(Line::from(""));

    for (i, tool) in turn.tool_invocations.iter().enumerate() {
        let is_selected = i == scroll_offset;
        let is_openable = is_subagent_tool(tool);
        let is_planning = is_planning_tool(tool);

        // Visual indicator for openable tools
        let marker = if is_selected {
            if is_openable {
                "▶ "
            } else {
                "● "
            }
        } else {
            "  "
        };

        let header_style = if is_selected {
            Style::default().fg(Color::Yellow).bold()
        } else if is_openable {
            Style::default().fg(Color::Magenta)
        } else if is_planning {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };

        // Tool label with context snippet
        let (tool_label, tool_context) = match &tool.tool_type {
            ToolType::Task {
                subagent_type,
                subagent_turns,
                description,
                ..
            } => {
                let type_info = subagent_type.as_deref().unwrap_or("Task");
                let label = if subagent_turns.is_empty() {
                    type_info.to_string()
                } else {
                    format!("{} ({} turns) ⏎", type_info, subagent_turns.len())
                };
                let context = truncate_str(description, 40);
                (label, context)
            }
            ToolType::FileRead { path, .. }
            | ToolType::FileWrite { path, .. }
            | ToolType::FileEdit { path, .. } => {
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
                let summary = if is_planning {
                    truncate_str(tool.input_display.lines().next().unwrap_or(""), 50)
                } else {
                    String::new()
                };
                (name.clone(), summary)
            }
        };

        let context_style = Style::default().fg(Color::DarkGray);
        let context_span = if tool_context.is_empty() {
            Span::raw("")
        } else {
            Span::styled(format!(" {tool_context}"), context_style)
        };
        let category_span = if is_openable {
            Span::styled(" [subagent]", Style::default().fg(Color::Magenta).bold())
        } else if is_planning {
            Span::styled(" [plan]", Style::default().fg(Color::Cyan).bold())
        } else {
            Span::raw("")
        };

        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(
                format!("[{}] ", i + 1),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(tool_label, header_style),
            category_span,
            context_span,
        ]));

        // Show details for selected tool
        if is_selected {
            lines.push(Line::from(""));
            if is_openable {
                lines.push(Line::styled(
                    "  Type: subagent call".to_string(),
                    Style::default().fg(Color::Magenta),
                ));
                lines.push(Line::from(""));
            } else if is_planning {
                lines.push(Line::styled(
                    "  Type: planning call".to_string(),
                    Style::default().fg(Color::Cyan),
                ));
                lines.push(Line::from(""));
            }

            // Input
            lines.push(Line::styled(
                "  Input:".to_string(),
                Style::default().fg(Color::Green),
            ));
            for line in tool.input_display.lines() {
                lines.push(Line::from(format!("    {line}")));
            }

            lines.push(Line::from(""));

            // Output
            lines.push(Line::styled(
                "  Output:".to_string(),
                Style::default().fg(Color::Yellow),
            ));
            for line in tool.output_display.lines().take(30) {
                lines.push(Line::from(format!("    {line}")));
            }
            if tool.output_display.lines().count() > 30 {
                lines.push(Line::styled(
                    "    ... (truncated)".to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            // Hint for openable tools
            if is_openable {
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    "  Press Enter to view subagent conversation".to_string(),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::ITALIC),
                ));
            }

            lines.push(Line::from(""));
        }
    }

    Text::from(lines)
}

fn is_subagent_tool(tool: &ToolInvocation) -> bool {
    matches!(&tool.tool_type, ToolType::Task { subagent_turns, .. } if !subagent_turns.is_empty())
}

fn is_planning_tool(tool: &ToolInvocation) -> bool {
    match &tool.tool_type {
        // Codex planning tool call
        ToolType::Other { name } => {
            name.eq_ignore_ascii_case("plan") || name.eq_ignore_ascii_case("update_plan")
        }
        // Claude planning/todo updates
        ToolType::TodoUpdate { .. } => true,
        _ => false,
    }
}

fn render_diff_inner(lines: &mut Vec<Line>, tool: &ToolInvocation, prefix: &str) -> bool {
    if let Some(diff) = tool.tool_type.diff() {
        let path = match &tool.tool_type {
            ToolType::FileEdit { path, .. } | ToolType::FileWrite { path, .. } => path.clone(),
            _ => "unknown".to_string(),
        };

        let header = if prefix.is_empty() {
            format!("─── {path} ───")
        } else {
            format!("─── {prefix} {path} ───")
        };

        lines.push(Line::styled(
            header,
            Style::default().fg(Color::Cyan).bold(),
        ));
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

fn render_diff_tab(turn: &Turn) -> Text<'static> {
    let mut lines: Vec<Line> = Vec::new();
    let mut has_diff = false;

    for tool in &turn.tool_invocations {
        if render_diff_inner(&mut lines, tool, "") {
            has_diff = true;
        }

        // Collect diffs from subagent turns
        if let ToolType::Task {
            subagent_turns,
            subagent_type,
            ..
        } = &tool.tool_type
        {
            if !subagent_turns.is_empty() {
                let prefix = format!("[{}]", subagent_type.as_deref().unwrap_or("subagent"));
                for subturn in subagent_turns {
                    for subtool in &subturn.tool_invocations {
                        if render_diff_inner(&mut lines, subtool, &prefix) {
                            has_diff = true;
                        }
                    }
                }
            }
        }
    }

    if !has_diff {
        return Text::styled(
            "No diffs available for this turn".to_string(),
            Style::default().fg(Color::DarkGray),
        );
    }

    Text::from(lines)
}

fn usage_text(bin: &str) -> String {
    format!(
        "Usage:\n  {bin}\n  {bin} import <shared-session.json.zst | share-url>\n  {bin} login\n  {bin} uploads\n  {bin} --help"
    )
}

fn parse_cli_command(args: &[String]) -> Result<CliCommand> {
    let bin = args.first().map_or("vibereview", String::as_str);
    match args {
        [_] => Ok(CliCommand::Browse),
        [_, flag] if flag == "-h" || flag == "--help" => Ok(CliCommand::Help),
        [_, cmd, path] if cmd == "import" => Ok(CliCommand::Import(PathBuf::from(path))),
        [_, cmd] if cmd == "login" => Ok(CliCommand::Login),
        [_, cmd] if cmd == "uploads" => Ok(CliCommand::Uploads),
        _ => Err(eyre!("Invalid arguments.\n\n{}", usage_text(bin))),
    }
}

fn load_imported_session(path: &Path) -> Result<Session> {
    if path.exists() {
        let shared = share::read_share_file_from_path(path)
            .map_err(|e| eyre!("Failed to read shared file {}: {e}", path.display()))?;
        return Ok(shared.session);
    }

    let input = path.to_string_lossy();
    if input.starts_with("http://") || input.starts_with("https://") {
        let shared = share::fetch_shared_session_from_cloud_link(&input)
            .map_err(|e| eyre!("Failed to import from share URL: {e}"))?;
        return Ok(shared.session);
    }

    Err(eyre!(
        "Import source not found: {} (expected local file or share URL)",
        path.display()
    ))
}

fn shell_quote_arg(arg: &str) -> String {
    format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
}

fn share_import_command(path: &str) -> String {
    format!("vibereview import {}", shell_quote_arg(path))
}

fn resolve_github_client_id() -> Result<String> {
    if let Ok(client_id) = std::env::var(auth::GITHUB_CLIENT_ID_ENV) {
        let client_id = client_id.trim().to_string();
        if !client_id.is_empty() {
            return Ok(client_id);
        }
    }
    share::fetch_github_client_id()
}

fn run_login_command() -> Result<()> {
    let client_id = resolve_github_client_id()?;
    let auth_state = auth::login_with_github(&client_id)?;
    auth::save_auth_state(&auth_state)?;

    println!(
        "Logged in as @{} (GitHub ID {}).",
        auth_state.github_login, auth_state.github_user_id
    );
    println!("Cloud uploads are now authorized for this account.");
    Ok(())
}

fn run_uploads_command() -> Result<()> {
    let token = auth::load_auth_token().ok_or_else(|| {
        eyre!(
            "Not logged in. Run `vibereview login` or set {}.",
            auth::GITHUB_TOKEN_ENV
        )
    })?;

    let uploads = share::list_uploads(&token)?;
    if uploads.uploads.is_empty() {
        println!("No uploads found for your GitHub account.");
        return Ok(());
    }

    for (idx, upload) in uploads.uploads.iter().enumerate() {
        let name = upload.session_name.as_deref().unwrap_or("session");
        let turns = upload
            .turn_count
            .map_or_else(|| "?".to_string(), |n| n.to_string());
        let short_fingerprint = &upload.fingerprint[..12.min(upload.fingerprint.len())];
        println!(
            "{}. {} | id={} | turns={} | security={} | fp={} | at={} | {}",
            idx + 1,
            name,
            upload.id,
            turns,
            upload.security,
            short_fingerprint,
            upload.uploaded_at,
            upload.url
        );
    }

    Ok(())
}

// =============================================================================
// Main
// =============================================================================

fn main() -> Result<()> {
    color_eyre::install()?;

    let args: Vec<String> = std::env::args().collect();
    let command = parse_cli_command(&args)?;
    match &command {
        CliCommand::Help => {
            let bin = args.first().map_or("vibereview", String::as_str);
            println!("{}", usage_text(bin));
            return Ok(());
        }
        CliCommand::Login => {
            run_login_command()?;
            return Ok(());
        }
        CliCommand::Uploads => {
            run_uploads_command()?;
            return Ok(());
        }
        CliCommand::Browse | CliCommand::Import(_) => {}
    }

    let imported_session = match command {
        CliCommand::Import(path) => Some(load_imported_session(&path)?),
        CliCommand::Browse | CliCommand::Help | CliCommand::Login | CliCommand::Uploads => None,
    };

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = match imported_session {
        Some(session) => App::with_open_session(session),
        None => App::new(),
    };

    let mut needs_redraw = true;

    while !app.should_quit {
        if needs_redraw {
            terminal.draw(|frame| ui(frame, &mut app))?;
            needs_redraw = false;
        }

        let timeout = app
            .next_timer_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(IDLE_POLL_TIMEOUT);

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        app.handle_key(key.code);
                        needs_redraw = true;
                    }
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse);
                    needs_redraw = true;
                }
                Event::Resize(_, _) => {
                    needs_redraw = true;
                }
                _ => {}
            }
        }

        if app.process_timers() {
            needs_redraw = true;
        }
    }

    io::stdout().execute(DisableMouseCapture)?;
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== SESSION GROUPING TESTS ====================

    #[test]
    fn test_session_grouping_by_slug() {
        // Test that list_all_sessions groups same-project sessions with the same slug
        let sessions = list_all_sessions();

        // Find sessions by slug
        let unified_exploring_gray: Vec<_> = sessions
            .iter()
            .filter(|s| s.slug.as_deref() == Some("unified-exploring-gray"))
            .collect();

        // Should be grouped into at most one entry per project
        let mut per_project: std::collections::HashMap<PathBuf, usize> =
            std::collections::HashMap::new();
        for session in &unified_exploring_gray {
            *per_project.entry(session.project_path.clone()).or_insert(0) += 1;
        }
        for (project, count) in per_project {
            assert!(
                count <= 1,
                "Sessions with same slug should be grouped per project: {:?} has {} entries",
                project,
                count
            );
        }

        // If such sessions exist, at least one should have multiple parts.
        if let Some(session) = unified_exploring_gray.iter().max_by_key(|s| s.part_count) {
            assert!(
                session.part_count >= 2,
                "unified-exploring-gray should have at least 2 parts, got {}",
                session.part_count
            );
            assert_eq!(
                session.paths.len(),
                session.part_count,
                "paths.len() should match part_count"
            );
        }
    }

    #[test]
    fn test_same_slug_different_projects_not_grouped() {
        let now = std::time::SystemTime::now();
        let parts = vec![
            SessionPart {
                source: Source::Claude,
                path: PathBuf::from("/tmp/project-a/a-1.jsonl"),
                name: "a-1".to_string(),
                project: "project-a".to_string(),
                project_path: PathBuf::from("/tmp/project-a"),
                modified: Some(now),
                description: Some("one".to_string()),
                slug: Some("shared-slug".to_string()),
            },
            SessionPart {
                source: Source::Claude,
                path: PathBuf::from("/tmp/project-b/b-1.jsonl"),
                name: "b-1".to_string(),
                project: "project-b".to_string(),
                project_path: PathBuf::from("/tmp/project-b"),
                modified: Some(now),
                description: Some("two".to_string()),
                slug: Some("shared-slug".to_string()),
            },
        ];

        let sessions = group_session_parts(parts);
        let grouped: Vec<_> = sessions
            .iter()
            .filter(|s| s.slug.as_deref() == Some("shared-slug"))
            .collect();

        assert_eq!(
            grouped.len(),
            2,
            "Same slug across different projects should not be merged"
        );
        assert!(grouped.iter().all(|s| s.part_count == 1));
    }

    #[test]
    fn test_sessions_without_slug_not_grouped() {
        let sessions = list_all_sessions();

        // Sessions without slugs should have part_count = 1
        for session in &sessions {
            if session.slug.is_none() {
                assert_eq!(
                    session.part_count, 1,
                    "Session without slug should have part_count=1: {}",
                    session.name
                );
                assert_eq!(
                    session.paths.len(),
                    1,
                    "Session without slug should have single path: {}",
                    session.name
                );
            }
        }
    }

    #[test]
    fn test_grouped_session_paths_sorted_chronologically() {
        let sessions = list_all_sessions();

        // For grouped sessions, paths should be sorted oldest to newest
        for session in &sessions {
            if session.part_count > 1 {
                // Verify paths are in chronological order by checking file modification times
                let mut prev_modified: Option<std::time::SystemTime> = None;
                for path in &session.paths {
                    if let Ok(metadata) = std::fs::metadata(path) {
                        if let Ok(modified) = metadata.modified() {
                            if let Some(prev) = prev_modified {
                                assert!(
                                    modified >= prev,
                                    "Paths should be sorted oldest to newest in session {}",
                                    session.name
                                );
                            }
                            prev_modified = Some(modified);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_unified_session_parse_single_file() {
        let sessions = list_all_sessions();

        // Find a session with single part
        if let Some(single_session) = sessions
            .iter()
            .find(|s| s.part_count == 1 && s.source == Source::Claude)
        {
            let result = single_session.parse();
            assert!(
                result.is_ok(),
                "Should parse single-file session: {:?}",
                result.err()
            );

            let session = result.unwrap();
            assert!(
                !session.turns.is_empty(),
                "Parsed session should have turns"
            );
        }
    }

    #[test]
    fn test_unified_session_parse_grouped_files() {
        let sessions = list_all_sessions();

        // Find a grouped session (multiple parts)
        if let Some(grouped_session) = sessions.iter().find(|s| s.part_count > 1) {
            let result = grouped_session.parse();
            assert!(
                result.is_ok(),
                "Should parse grouped session: {:?}",
                result.err()
            );

            let session = result.unwrap();
            assert!(
                !session.turns.is_empty(),
                "Parsed grouped session should have turns"
            );

            // The combined session should have more turns than individual files
            // (This is a sanity check - actual count depends on the specific sessions)
        }
    }

    #[test]
    fn test_no_duplicate_sessions_in_list() {
        let sessions = list_all_sessions();

        // Check that no two sessions have overlapping paths
        let mut all_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for session in &sessions {
            for path in &session.paths {
                assert!(
                    all_paths.insert(path.clone()),
                    "Path {:?} appears in multiple sessions",
                    path
                );
            }
        }
    }

    #[test]
    fn test_session_display_with_part_count() {
        // Test that grouped sessions would display correctly with ×N indicator
        let sessions = list_all_sessions();

        for session in &sessions {
            let id = session.list_id();

            // Verify format
            if session.part_count > 1 {
                assert!(
                    id.contains('×'),
                    "Grouped session should have × indicator: {}",
                    id
                );
            } else {
                assert!(
                    !id.contains('×'),
                    "Single session should not have × indicator: {}",
                    id
                );
            }
        }
    }

    #[test]
    fn test_codex_list_id_strips_rollout_prefix() {
        let session = UnifiedSession {
            source: Source::Codex,
            paths: Vec::new(),
            name: "rollout-abcdef1234567890".to_string(),
            project: "test".to_string(),
            project_path: PathBuf::from("/tmp/test"),
            modified: None,
            description: None,
            slug: None,
            part_count: 1,
        };

        assert_eq!(session.list_id(), "abcdef123456");
    }

    // ==================== RESUME COMMAND TESTS ====================

    #[test]
    fn test_resume_session_id_single_file() {
        let sessions = list_all_sessions();

        // For single-file sessions:
        // - Claude: resume_session_id should be the filename stem (UUID)
        // - Codex: resume_session_id should be just the UUID extracted from filename
        for session in &sessions {
            if session.part_count == 1 {
                let resume_id = session.resume_session_id();
                let filename_stem = session.paths[0]
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();

                match session.source {
                    Source::Claude => {
                        assert_eq!(
                            resume_id, filename_stem,
                            "Claude resume ID should match filename stem"
                        );
                    }
                    Source::Codex => {
                        // Codex extracts just the UUID from "rollout-DATE-UUID"
                        assert!(
                            filename_stem.contains(&resume_id),
                            "Codex filename should contain the resume UUID: {} not in {}",
                            resume_id,
                            filename_stem
                        );
                        assert!(
                            !resume_id.contains("rollout"),
                            "Codex resume ID should not contain 'rollout'"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_resume_session_id_grouped_files() {
        let sessions = list_all_sessions();

        // For grouped sessions, resume_session_id should be the LAST file's ID (most recent)
        for session in &sessions {
            if session.part_count > 1 {
                let resume_id = session.resume_session_id();
                let expected_id = session
                    .paths
                    .last()
                    .and_then(|p| p.file_stem())
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                assert_eq!(
                    resume_id, expected_id,
                    "Resume ID for grouped session should be the most recent file's ID"
                );
                // The resume ID should NOT be the first session's name (which is used for display)
                // unless there's only one file
                assert_ne!(
                    resume_id, session.name,
                    "Resume ID should be different from display name for grouped sessions"
                );
            }
        }
    }

    #[test]
    fn test_get_resume_command_claude() {
        let sessions = list_all_sessions();

        for session in &sessions {
            if session.source == Source::Claude {
                let cmd = session.get_resume_command();

                // Should start with cd and include claude --resume
                assert!(
                    cmd.starts_with("cd "),
                    "Claude resume command should start with 'cd ': {}",
                    cmd
                );
                assert!(
                    cmd.contains("claude --resume"),
                    "Claude resume command should contain 'claude --resume': {}",
                    cmd
                );

                // Should contain the project path
                let project_path = session.project_path.display().to_string();
                assert!(
                    cmd.contains(&project_path),
                    "Resume command should contain project path '{}': {}",
                    project_path,
                    cmd
                );

                // Should contain the resume session ID
                let resume_id = session.resume_session_id();
                assert!(
                    cmd.contains(&resume_id),
                    "Resume command should contain session ID '{}': {}",
                    resume_id,
                    cmd
                );
            }
        }
    }

    #[test]
    fn test_get_resume_command_codex() {
        let sessions = list_all_sessions();

        for session in &sessions {
            if session.source == Source::Codex {
                let cmd = session.get_resume_command();
                let resume_id = session.resume_session_id();

                // Should start with cd and include codex resume (not --resume)
                assert!(
                    cmd.starts_with("cd "),
                    "Codex resume command should start with 'cd ': {}",
                    cmd
                );
                assert!(
                    cmd.contains("codex resume"),
                    "Codex resume command should contain 'codex resume': {}",
                    cmd
                );
                assert!(
                    !cmd.contains("--resume"),
                    "Codex resume command should NOT contain '--resume': {}",
                    cmd
                );
                // Resume ID should be just the UUID (not contain "rollout" or date)
                assert!(
                    !resume_id.contains("rollout"),
                    "Codex resume ID should not contain 'rollout': {}",
                    resume_id
                );
                assert!(
                    !resume_id.contains("T"),
                    "Codex resume ID should not contain datetime 'T': {}",
                    resume_id
                );
                // UUID format: 8-4-4-4-12 hex characters
                let uuid_parts: Vec<&str> = resume_id.split('-').collect();
                assert_eq!(
                    uuid_parts.len(),
                    5,
                    "Codex resume ID should have 5 UUID parts: {}",
                    resume_id
                );
            }
        }
    }

    #[test]
    fn test_session_has_project_path() {
        let sessions = list_all_sessions();

        for session in &sessions {
            // All sessions should have a valid project path
            assert!(
                !session.project_path.as_os_str().is_empty(),
                "Session {} should have a non-empty project path",
                session.name
            );

            // Claude sessions should have decoded project paths (starting with /)
            if session.source == Source::Claude {
                assert!(
                    session.project_path.is_absolute(),
                    "Claude session project path should be absolute: {:?}",
                    session.project_path
                );
            }
        }
    }

    #[test]
    fn test_tool_kind_classification_planning_and_subagent() {
        let planning_tool = ToolInvocation {
            id: "plan-1".to_string(),
            tool_type: ToolType::Other {
                name: "Plan".to_string(),
            },
            input_display: "1. inspect\n2. implement".to_string(),
            output_display: "Plan updated".to_string(),
            raw_input: serde_json::Value::Null,
            raw_output: None,
        };
        assert!(is_planning_tool(&planning_tool));
        assert!(!is_subagent_tool(&planning_tool));

        let subagent_tool = ToolInvocation {
            id: "task-1".to_string(),
            tool_type: ToolType::Task {
                description: "delegate".to_string(),
                prompt: "do work".to_string(),
                subagent_type: Some("Explore".to_string()),
                result: Some("ok".to_string()),
                subagent_turns: vec![Turn {
                    id: "subturn-1".to_string(),
                    timestamp: None,
                    user_prompt: "check".to_string(),
                    thinking: None,
                    thinking_effort: None,
                    tool_invocations: Vec::new(),
                    response: "done".to_string(),
                    model: None,
                }],
            },
            input_display: "delegate".to_string(),
            output_display: "ok".to_string(),
            raw_input: serde_json::Value::Null,
            raw_output: None,
        };
        assert!(is_subagent_tool(&subagent_tool));
        assert!(!is_planning_tool(&subagent_tool));
    }

    #[test]
    fn test_parse_cli_command_defaults_to_browse() {
        let args = vec!["vibereview".to_string()];
        let parsed = parse_cli_command(&args).unwrap();
        assert!(matches!(parsed, CliCommand::Browse));
    }

    #[test]
    fn test_parse_cli_command_import() {
        let args = vec![
            "vibereview".to_string(),
            "import".to_string(),
            "/tmp/share.json.zst".to_string(),
        ];
        let parsed = parse_cli_command(&args).unwrap();
        match parsed {
            CliCommand::Import(path) => {
                assert_eq!(path.display().to_string(), "/tmp/share.json.zst")
            }
            _ => panic!("expected import command"),
        }
    }

    #[test]
    fn test_parse_cli_command_login() {
        let args = vec!["vibereview".to_string(), "login".to_string()];
        let parsed = parse_cli_command(&args).unwrap();
        assert!(matches!(parsed, CliCommand::Login));
    }

    #[test]
    fn test_parse_cli_command_uploads() {
        let args = vec!["vibereview".to_string(), "uploads".to_string()];
        let parsed = parse_cli_command(&args).unwrap();
        assert!(matches!(parsed, CliCommand::Uploads));
    }

    #[test]
    fn test_share_import_command_quotes_path() {
        let cmd = share_import_command("/tmp/session file.json.zst");
        assert_eq!(cmd, "vibereview import \"/tmp/session file.json.zst\"");
    }

    #[test]
    fn test_share_import_command_quotes_url_with_fragment() {
        let cmd = share_import_command("https://share.example/s/abc123DEF_45#k=secret");
        assert_eq!(
            cmd,
            "vibereview import \"https://share.example/s/abc123DEF_45#k=secret\""
        );
    }

    #[test]
    fn test_wrap_text_for_selection_wraps_lines() {
        let wrapped = wrap_text_for_selection("abcdefgh", 4);
        assert_eq!(wrapped, vec!["abcd", "efgh"]);
    }

    #[test]
    fn test_wrap_text_for_selection_preserves_empty_lines() {
        let wrapped = wrap_text_for_selection("ab\n\ncd", 8);
        assert_eq!(wrapped, vec!["ab", "", "cd"]);
    }

    #[test]
    fn test_browser_help_text_hides_resume_when_unavailable() {
        assert!(!browser_help_text(false).contains("R: Resume"));
        assert!(browser_help_text(true).contains("R: Resume"));
    }

    #[test]
    fn test_viewer_help_text_hides_resume_when_unavailable() {
        assert!(!viewer_help_text(false, false).contains("R: Resume"));
        assert!(!viewer_help_text(true, false).contains("R: Resume"));
        assert!(viewer_help_text(false, true).contains("R: Resume"));
        assert!(viewer_help_text(true, true).contains("R: Resume"));
    }

    #[test]
    fn test_help_text_mentions_metadata_shortcut() {
        assert!(browser_help_text(false).contains("m: Metadata"));
        assert!(viewer_help_text(false, false).contains("m: Metadata"));
        assert!(viewer_help_text(true, false).contains("m: Metadata"));
    }

    #[test]
    fn test_build_turn_metadata_report_includes_model_and_effort() {
        let session = Session {
            id: "s1".to_string(),
            name: "test".to_string(),
            source: models::SessionSource::Other {
                name: "x".to_string(),
            },
            project_path: None,
            turns: vec![
                Turn {
                    id: "t1".to_string(),
                    timestamp: None,
                    user_prompt: "first prompt".to_string(),
                    thinking: None,
                    thinking_effort: Some("high".to_string()),
                    tool_invocations: Vec::new(),
                    response: "r1".to_string(),
                    model: Some("gpt-5.3-codex".to_string()),
                },
                Turn {
                    id: "t2".to_string(),
                    timestamp: None,
                    user_prompt: "second prompt".to_string(),
                    thinking: None,
                    thinking_effort: None,
                    tool_invocations: Vec::new(),
                    response: "r2".to_string(),
                    model: None,
                },
            ],
        };

        let report = build_turn_metadata_report(&session);
        assert!(report.contains("model=gpt-5.3-codex"));
        assert!(report.contains("thinking_effort=high"));
        assert!(report.contains("model=-"));
        assert!(report.contains("thinking_effort=-"));
    }
}
