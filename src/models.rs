//! Generic data models for session transcripts.
//! Designed to support multiple sources (Claude Code, other LLM tools, etc.)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use similar::TextDiff;

/// A session contains metadata and a list of conversation turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub source: SessionSource,
    pub project_path: Option<PathBuf>,
    pub turns: Vec<Turn>,
}

/// Where this session originated from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionSource {
    ClaudeCode { version: Option<String> },
    Other { name: String },
}

/// A single turn in the conversation (user prompt + model response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: String,
    pub timestamp: Option<String>,
    pub user_prompt: String,
    pub thinking: Option<String>,
    pub tool_invocations: Vec<ToolInvocation>,
    pub response: String,
    pub model: Option<String>,
}

/// A tool invocation with its input, output, and type information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub id: String,
    pub tool_type: ToolType,
    pub input_display: String,
    pub output_display: String,
    pub raw_input: serde_json::Value,
    pub raw_output: Option<serde_json::Value>,
}

/// Categorized tool types for rendering purposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolType {
    FileRead {
        path: String,
        content: Option<String>,
    },
    FileWrite {
        path: String,
        content: String,
    },
    FileEdit {
        path: String,
        old_content: Option<String>,
        new_content: Option<String>,
        diff: Option<String>,
    },
    Command {
        command: String,
        stdout: Option<String>,
        stderr: Option<String>,
        exit_code: Option<i32>,
    },
    Search {
        pattern: String,
        results: Vec<String>,
    },
    WebFetch {
        url: String,
        content: Option<String>,
    },
    WebSearch {
        query: String,
        results: Option<String>,
    },
    TodoUpdate {
        todos: Vec<TodoItem>,
    },
    Task {
        description: String,
        prompt: String,
        subagent_type: Option<String>,
        result: Option<String>,
        /// Embedded subagent turns (parsed from agent-{id}.jsonl)
        subagent_turns: Vec<Turn>,
    },
    Other {
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

impl ToolType {
    pub fn name(&self) -> &str {
        match self {
            Self::FileRead { .. } => "Read",
            Self::FileWrite { .. } => "Write",
            Self::FileEdit { .. } => "Edit",
            Self::Command { .. } => "Bash",
            Self::Search { .. } => "Search",
            Self::WebFetch { .. } => "WebFetch",
            Self::WebSearch { .. } => "WebSearch",
            Self::TodoUpdate { .. } => "TodoWrite",
            Self::Task { .. } => "Task",
            Self::Other { name } => name,
        }
    }

    /// Returns a diff string if this tool type has one.
    pub fn diff(&self) -> Option<String> {
        match self {
            Self::FileEdit { diff, old_content, new_content, .. } => {
                if let Some(d) = diff {
                    return Some(d.clone());
                }
                // Generate a simple diff if we have old and new content
                if let (Some(old), Some(new)) = (old_content, new_content) {
                    Some(generate_simple_diff(old, new))
                } else {
                    None
                }
            }
            Self::FileWrite { content, path } => {
                // Show as all additions
                let lines: Vec<String> = content
                    .lines()
                    .map(|l| format!("+{l}"))
                    .collect();
                Some(format!("--- /dev/null\n+++ {}\n{}", path, lines.join("\n")))
            }
            _ => None,
        }
    }
}

fn generate_simple_diff(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header("old", "new")
        .to_string()
}
