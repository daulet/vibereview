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

// =============================================================================
// Data Models
// =============================================================================

#[derive(Debug, Clone)]
pub struct ChangeNode {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub thinking: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub diff: Option<String>,
    pub children: Vec<ChangeNode>,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub input: String,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct FlattenedNode {
    pub node: ChangeNode,
    pub depth: usize,
    pub is_last_child: bool,
    pub parent_is_last: Vec<bool>,
}

// =============================================================================
// App State
// =============================================================================

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
    pub tree: Vec<ChangeNode>,
    pub flattened: Vec<FlattenedNode>,
    pub list_state: ListState,
    pub active_tab: DetailTab,
    pub scroll_offset: u16,
    pub should_quit: bool,
}

impl App {
    pub fn new(tree: Vec<ChangeNode>) -> Self {
        let flattened = flatten_tree(&tree);
        let mut list_state = ListState::default();
        if !flattened.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            tree,
            flattened,
            list_state,
            active_tab: DetailTab::Prompt,
            scroll_offset: 0,
            should_quit: false,
        }
    }

    pub fn selected_node(&self) -> Option<&FlattenedNode> {
        self.list_state.selected().and_then(|i| self.flattened.get(i))
    }

    pub fn select_next(&mut self) {
        if self.flattened.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i >= self.flattened.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
        self.scroll_offset = 0;
    }

    pub fn select_prev(&mut self) {
        if self.flattened.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.flattened.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
        self.scroll_offset = 0;
    }

    pub fn next_tab(&mut self) {
        self.active_tab = self.active_tab.next();
        self.scroll_offset = 0;
    }

    pub fn prev_tab(&mut self) {
        self.active_tab = self.active_tab.prev();
        self.scroll_offset = 0;
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }
}

fn flatten_tree(tree: &[ChangeNode]) -> Vec<FlattenedNode> {
    let mut result = Vec::new();
    flatten_recursive(tree, 0, &mut vec![], &mut result);
    result
}

fn flatten_recursive(
    nodes: &[ChangeNode],
    depth: usize,
    parent_is_last: &mut Vec<bool>,
    result: &mut Vec<FlattenedNode>,
) {
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i == nodes.len() - 1;
        result.push(FlattenedNode {
            node: node.clone(),
            depth,
            is_last_child: is_last,
            parent_is_last: parent_is_last.clone(),
        });

        if !node.children.is_empty() {
            parent_is_last.push(is_last);
            flatten_recursive(&node.children, depth + 1, parent_is_last, result);
            parent_is_last.pop();
        }
    }
}

// =============================================================================
// UI Rendering
// =============================================================================

fn ui(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(frame.area());

    render_tree(frame, app, chunks[0]);
    render_detail_panel(frame, app, chunks[1]);
}

fn render_tree(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .flattened
        .iter()
        .map(|flat_node| {
            let mut prefix = String::new();

            for (i, &is_last) in flat_node.parent_is_last.iter().enumerate() {
                if i < flat_node.depth {
                    if is_last {
                        prefix.push_str("    ");
                    } else {
                        prefix.push_str("│   ");
                    }
                }
            }

            if flat_node.depth > 0 {
                if flat_node.is_last_child {
                    prefix.push_str("└── ");
                } else {
                    prefix.push_str("├── ");
                }
            }

            let content = format!("{}{}", prefix, flat_node.node.title);
            ListItem::new(content)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Changes Tree "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_detail_panel(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

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

    let content_block = Block::default().borders(Borders::ALL);

    if let Some(flat_node) = app.selected_node() {
        let node = &flat_node.node;
        let content: Text = match app.active_tab {
            DetailTab::Prompt => Text::from(node.prompt.clone()),
            DetailTab::Thinking => {
                if let Some(thinking) = &node.thinking {
                    Text::from(thinking.clone())
                } else {
                    Text::styled("No thinking available", Style::default().fg(Color::DarkGray))
                }
            }
            DetailTab::ToolCalls => {
                if node.tool_calls.is_empty() {
                    Text::styled("No tool calls", Style::default().fg(Color::DarkGray))
                } else {
                    let mut lines: Vec<Line> = Vec::new();
                    for (i, tc) in node.tool_calls.iter().enumerate() {
                        if i > 0 {
                            lines.push(Line::from(""));
                            lines.push(Line::from("─".repeat(40)));
                            lines.push(Line::from(""));
                        }
                        lines.push(Line::from(vec![
                            Span::styled("Tool: ", Style::default().fg(Color::Cyan)),
                            Span::styled(&tc.tool_name, Style::default().bold()),
                        ]));
                        lines.push(Line::from(""));
                        lines.push(Line::styled("Input:", Style::default().fg(Color::Green)));
                        for line in tc.input.lines() {
                            lines.push(Line::from(format!("  {}", line)));
                        }
                        lines.push(Line::from(""));
                        lines.push(Line::styled("Output:", Style::default().fg(Color::Yellow)));
                        for line in tc.output.lines() {
                            lines.push(Line::from(format!("  {}", line)));
                        }
                    }
                    Text::from(lines)
                }
            }
            DetailTab::Diff => {
                if let Some(diff) = &node.diff {
                    let lines: Vec<Line> = diff
                        .lines()
                        .map(|line| {
                            if line.starts_with('+') && !line.starts_with("+++") {
                                Line::styled(line, Style::default().fg(Color::Green))
                            } else if line.starts_with('-') && !line.starts_with("---") {
                                Line::styled(line, Style::default().fg(Color::Red))
                            } else if line.starts_with("@@") {
                                Line::styled(line, Style::default().fg(Color::Cyan))
                            } else {
                                Line::from(line)
                            }
                        })
                        .collect();
                    Text::from(lines)
                } else {
                    Text::styled("No diff available", Style::default().fg(Color::DarkGray))
                }
            }
        };

        let paragraph = Paragraph::new(content)
            .block(content_block)
            .wrap(Wrap { trim: false })
            .scroll((app.scroll_offset, 0));
        frame.render_widget(paragraph, chunks[1]);
    } else {
        let paragraph = Paragraph::new("Select a change to view details")
            .block(content_block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, chunks[1]);
    }

    // Render help at the bottom
    let help_text = " ↑/↓: Navigate | ←/→: Tabs | j/k: Scroll | q: Quit ";
    let help_area = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    let help = Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(help, help_area);
}

// =============================================================================
// Sample Data
// =============================================================================

fn create_sample_tree() -> Vec<ChangeNode> {
    vec![
        ChangeNode {
            id: "1".to_string(),
            title: "Initial project setup".to_string(),
            prompt: "Create a new Rust project with ratatui for building a terminal UI application that displays a tree of changes.".to_string(),
            thinking: Some("The user wants to create a TUI application. I should:\n1. Initialize a new Cargo project\n2. Add ratatui and crossterm dependencies\n3. Set up the basic terminal handling\n4. Create a simple render loop".to_string()),
            tool_calls: vec![
                ToolCall {
                    tool_name: "Bash".to_string(),
                    input: "cargo init promptui".to_string(),
                    output: "Created binary (application) `promptui` package".to_string(),
                },
                ToolCall {
                    tool_name: "Edit".to_string(),
                    input: "Add dependencies to Cargo.toml".to_string(),
                    output: "Added ratatui = \"0.29\"\nAdded crossterm = \"0.28\"".to_string(),
                },
            ],
            diff: Some(r#"diff --git a/Cargo.toml b/Cargo.toml
new file mode 100644
--- /dev/null
+++ b/Cargo.toml
@@ -0,0 +1,9 @@
+[package]
+name = "promptui"
+version = "0.1.0"
+edition = "2021"
+
+[dependencies]
+ratatui = "0.29"
+crossterm = "0.28"
"#.to_string()),
            children: vec![
                ChangeNode {
                    id: "1.1".to_string(),
                    title: "Add data models".to_string(),
                    prompt: "Define the data structures for representing a tree of changes with prompts, thinking, and tool calls.".to_string(),
                    thinking: Some("I need to create structs that can:\n- Represent a tree structure (nodes with children)\n- Store LLM interaction data (prompt, thinking, tool calls)\n- Store diffs for code changes\n\nI'll create ChangeNode and ToolCall structs.".to_string()),
                    tool_calls: vec![
                        ToolCall {
                            tool_name: "Edit".to_string(),
                            input: "Add ChangeNode and ToolCall structs to main.rs".to_string(),
                            output: "Structs added successfully".to_string(),
                        },
                    ],
                    diff: Some(r#"diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,0 +1,20 @@
+#[derive(Debug, Clone)]
+pub struct ChangeNode {
+    pub id: String,
+    pub title: String,
+    pub prompt: String,
+    pub thinking: Option<String>,
+    pub tool_calls: Vec<ToolCall>,
+    pub diff: Option<String>,
+    pub children: Vec<ChangeNode>,
+}
+
+#[derive(Debug, Clone)]
+pub struct ToolCall {
+    pub tool_name: String,
+    pub input: String,
+    pub output: String,
+}
"#.to_string()),
                    children: vec![],
                    depth: 1,
                },
                ChangeNode {
                    id: "1.2".to_string(),
                    title: "Implement tree view".to_string(),
                    prompt: "Create the left panel that displays the changes as a tree with proper indentation and branch characters.".to_string(),
                    thinking: Some("For the tree view, I need to:\n1. Flatten the tree for display in a list\n2. Calculate proper indentation with tree characters (├── └── │)\n3. Track parent state to draw connecting lines correctly\n4. Handle selection highlighting".to_string()),
                    tool_calls: vec![
                        ToolCall {
                            tool_name: "Edit".to_string(),
                            input: "Add flatten_tree function and render_tree".to_string(),
                            output: "Tree rendering implemented".to_string(),
                        },
                    ],
                    diff: None,
                    children: vec![
                        ChangeNode {
                            id: "1.2.1".to_string(),
                            title: "Add tree navigation".to_string(),
                            prompt: "Implement keyboard navigation for moving up and down in the tree.".to_string(),
                            thinking: Some("Navigation should:\n- Support arrow keys (up/down)\n- Wrap around at boundaries\n- Reset scroll position when selection changes".to_string()),
                            tool_calls: vec![],
                            diff: None,
                            children: vec![],
                            depth: 2,
                        },
                    ],
                    depth: 1,
                },
            ],
            depth: 0,
        },
        ChangeNode {
            id: "2".to_string(),
            title: "Add detail panel".to_string(),
            prompt: "Create the right panel with tabs for viewing prompt, thinking, tool calls, and diff.".to_string(),
            thinking: Some("The detail panel needs:\n1. A tab bar at the top for switching views\n2. Content area that changes based on selected tab\n3. Syntax highlighting for diffs (green for additions, red for deletions)\n4. Scrolling support for long content".to_string()),
            tool_calls: vec![
                ToolCall {
                    tool_name: "Edit".to_string(),
                    input: "Add DetailTab enum and render_detail_panel function".to_string(),
                    output: "Detail panel with tabs implemented".to_string(),
                },
            ],
            diff: Some(r#"diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -50,0 +51,15 @@
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub enum DetailTab {
+    Prompt,
+    Thinking,
+    ToolCalls,
+    Diff,
+}
"#.to_string()),
            children: vec![
                ChangeNode {
                    id: "2.1".to_string(),
                    title: "Style diff output".to_string(),
                    prompt: "Add syntax highlighting to the diff view with colors for additions and deletions.".to_string(),
                    thinking: None,
                    tool_calls: vec![
                        ToolCall {
                            tool_name: "Edit".to_string(),
                            input: "Add color styling for diff lines".to_string(),
                            output: "Green for +, Red for -, Cyan for @@".to_string(),
                        },
                    ],
                    diff: Some(r#"diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -180,6 +180,14 @@
-                    Text::from(diff.clone())
+                    let lines: Vec<Line> = diff
+                        .lines()
+                        .map(|line| {
+                            if line.starts_with('+') {
+                                Line::styled(line, Style::default().fg(Color::Green))
+                            } else if line.starts_with('-') {
+                                Line::styled(line, Style::default().fg(Color::Red))
+                            } else {
+                                Line::from(line)
+                            }
+                        })
+                        .collect();
+                    Text::from(lines)
"#.to_string()),
                    children: vec![],
                    depth: 1,
                },
            ],
            depth: 0,
        },
        ChangeNode {
            id: "3".to_string(),
            title: "Bug fix: scroll reset".to_string(),
            prompt: "Fix the bug where scroll position wasn't resetting when changing selection or tabs.".to_string(),
            thinking: Some("The scroll_offset needs to be reset to 0 whenever:\n1. The user selects a different node in the tree\n2. The user switches tabs\n\nI'll add scroll_offset = 0 to select_next, select_prev, next_tab, and prev_tab methods.".to_string()),
            tool_calls: vec![],
            diff: Some(r#"diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -95,6 +95,7 @@
         };
         self.list_state.select(Some(i));
+        self.scroll_offset = 0;
     }
"#.to_string()),
            children: vec![],
            depth: 0,
        },
    ]
}

// =============================================================================
// Main & Event Loop
// =============================================================================

fn main() -> Result<()> {
    color_eyre::install()?;

    // Setup terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // Create app with sample data
    let tree = create_sample_tree();
    let mut app = App::new(tree);

    // Main loop
    while !app.should_quit {
        terminal.draw(|frame| ui(frame, &mut app))?;

        if event::poll(std::time::Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                        KeyCode::Up => app.select_prev(),
                        KeyCode::Down => app.select_next(),
                        KeyCode::Left => app.prev_tab(),
                        KeyCode::Right => app.next_tab(),
                        KeyCode::Char('j') => app.scroll_down(),
                        KeyCode::Char('k') => app.scroll_up(),
                        KeyCode::Tab => app.next_tab(),
                        _ => {}
                    }
                }
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}
