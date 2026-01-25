//! Codex CLI session parser.
//! Handles both old JSON format and new JSONL format.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::models::{Session, SessionSource, ToolInvocation, ToolType, Turn};

/// Information about a Codex project (derived from session cwd).
#[derive(Debug, Clone)]
pub struct CodexProjectInfo {
    pub path: PathBuf,
    pub name: String,
}

/// Information about a Codex session file.
#[derive(Debug, Clone)]
pub struct CodexSessionInfo {
    pub path: PathBuf,
    pub name: String,
    pub modified: Option<SystemTime>,
    pub project_path: Option<PathBuf>,
    pub description: Option<String>,
}

/// List all Codex sessions from ~/.codex/sessions/
pub fn list_codex_sessions() -> Vec<CodexSessionInfo> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let sessions_dir = home.join(".codex/sessions");
    if !sessions_dir.exists() {
        return Vec::new();
    }

    let mut sessions = Vec::new();
    collect_sessions_recursive(&sessions_dir, &mut sessions);

    // Sort by modification time, newest first
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    sessions
}

fn collect_sessions_recursive(dir: &Path, sessions: &mut Vec<CodexSessionInfo>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sessions_recursive(&path, sessions);
        } else if path.is_file() {
            let name = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            // Only include .json and .jsonl files that have actual content
            if name.ends_with(".json") || name.ends_with(".jsonl") {
                // Skip empty sessions (no actual user messages)
                if !codex_session_has_messages(&path) {
                    continue;
                }
                let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
                let project_path = extract_session_project_path(&path);
                let description = extract_codex_session_description(&path);
                sessions.push(CodexSessionInfo {
                    path,
                    name,
                    modified,
                    project_path,
                    description,
                });
            }
        }
    }
}

/// Extract description from first user message in Codex session.
fn extract_codex_session_description(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    for line in reader.lines().take(50).flatten() {
        // JSONL format: event_msg with user_message
        if line.contains("\"user_message\"") {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if let Some(message) = value
                    .get("payload")
                    .and_then(|p| p.get("message"))
                    .and_then(|m| m.as_str())
                {
                    return Some(message.to_string());
                }
            }
        }
        // JSON format: message with role user and input_text content
        if line.contains("\"role\":\"user\"") || line.contains("\"role\": \"user\"") {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                // Try content array with input_text
                if let Some(content) = value.get("content").and_then(|c| c.as_array()) {
                    for item in content {
                        if item.get("type").and_then(|t| t.as_str()) == Some("input_text") {
                            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                return Some(text.to_string());
                            }
                        }
                    }
                }
                // Try simple string content
                if let Some(text) = value.get("content").and_then(|c| c.as_str()) {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

/// Quick check if a Codex session file has actual conversation (not just metadata).
fn codex_session_has_messages(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let reader = BufReader::new(file);

    // A real conversation has agent/assistant responses - check for those
    for line in reader.lines().take(100).flatten() {
        // JSONL format: event_msg with agent_message type
        if line.contains("\"agent_message\"") {
            return true;
        }
        // JSON format: message with role assistant
        if line.contains("\"role\":\"assistant\"") || line.contains("\"role\": \"assistant\"") {
            return true;
        }
        // Also check for function calls which indicate actual work
        if line.contains("\"function_call\"") {
            return true;
        }
    }
    false
}

/// Extract project path (cwd) from session file without fully parsing it.
fn extract_session_project_path(path: &Path) -> Option<PathBuf> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    // For JSONL, look for session_meta line
    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
        for line in reader.lines().take(10) {
            // Only check first 10 lines
            let line = line.ok()?;
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if value.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
                    return value
                        .get("payload")
                        .and_then(|p| p.get("cwd"))
                        .and_then(|c| c.as_str())
                        .map(PathBuf::from);
                }
            }
        }
    }
    // Old JSON format doesn't have cwd
    None
}

/// List Codex projects (unique cwd paths from sessions).
pub fn list_codex_projects() -> Vec<CodexProjectInfo> {
    let sessions = list_codex_sessions();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for session in &sessions {
        if let Some(proj_path) = &session.project_path {
            seen.insert(proj_path.clone());
        }
    }

    let mut projects: Vec<CodexProjectInfo> = seen
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            CodexProjectInfo { path, name }
        })
        .collect();

    // Sort by name
    projects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    projects
}

/// List Codex sessions for a specific project path.
pub fn list_codex_sessions_for_project(project_path: &Path) -> Vec<CodexSessionInfo> {
    let sessions = list_codex_sessions();
    let mut filtered: Vec<CodexSessionInfo> = sessions
        .into_iter()
        .filter(|s| s.project_path.as_deref() == Some(project_path))
        .collect();

    // Sort by modification time, newest first
    filtered.sort_by(|a, b| b.modified.cmp(&a.modified));
    filtered
}

/// Parse a Codex session file (supports both JSON and JSONL formats).
pub fn parse_codex_session(path: &Path) -> Result<Session, String> {
    let name = path.file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let extension = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    match extension {
        "jsonl" => parse_jsonl_session(path, &name),
        "json" => parse_json_session(path, &name),
        _ => Err(format!("Unknown session format: {}", extension)),
    }
}

/// Parse new JSONL format session.
fn parse_jsonl_session(path: &Path, name: &str) -> Result<Session, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open file: {}", e))?;
    let reader = BufReader::new(file);

    let mut session_id = name.to_string();
    let mut cli_version = None;
    let mut model = None;
    let mut project_path = None;

    let mut entries: Vec<Value> = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|e| format!("Failed to read line: {}", e))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            // Extract metadata from session_meta
            if value.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
                if let Some(payload) = value.get("payload") {
                    if let Some(id) = payload.get("id").and_then(|i| i.as_str()) {
                        session_id = id.to_string();
                    }
                    if let Some(version) = payload.get("cli_version").and_then(|v| v.as_str()) {
                        cli_version = Some(version.to_string());
                    }
                    if let Some(cwd) = payload.get("cwd").and_then(|c| c.as_str()) {
                        project_path = Some(PathBuf::from(cwd));
                    }
                }
            }
            // Extract model from turn_context
            if value.get("type").and_then(|t| t.as_str()) == Some("turn_context") {
                if let Some(payload) = value.get("payload") {
                    if let Some(m) = payload.get("model").and_then(|m| m.as_str()) {
                        model = Some(m.to_string());
                    }
                }
            }
            entries.push(value);
        }
    }

    let turns = build_turns_from_jsonl(&entries, model.as_deref());

    Ok(Session {
        id: session_id,
        name: name.to_string(),
        source: SessionSource::Other {
            name: format!("Codex {}", cli_version.as_deref().unwrap_or("CLI")),
        },
        project_path,
        turns,
    })
}

/// Parse old JSON format session.
fn parse_json_session(path: &Path, name: &str) -> Result<Session, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read file: {}", e))?;

    let data: Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse JSON: {}", e))?;

    let session_id = data.get("session")
        .and_then(|s| s.get("id"))
        .and_then(|i| i.as_str())
        .unwrap_or(name)
        .to_string();

    let items = data.get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();

    let turns = build_turns_from_json(&items);

    Ok(Session {
        id: session_id,
        name: name.to_string(),
        source: SessionSource::Other {
            name: "Codex CLI (legacy)".to_string(),
        },
        project_path: None,
        turns,
    })
}

/// Build turns from JSONL entries.
fn build_turns_from_jsonl(entries: &[Value], default_model: Option<&str>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut current_turn: Option<Turn> = None;
    let mut pending_calls: HashMap<String, Value> = HashMap::new();

    for entry in entries {
        let entry_type = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let timestamp = entry.get("timestamp").and_then(|t| t.as_str()).map(String::from);

        match entry_type {
            "event_msg" => {
                if let Some(payload) = entry.get("payload") {
                    let msg_type = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match msg_type {
                        "user_message" => {
                            // Finalize previous turn
                            if let Some(turn) = current_turn.take() {
                                if !turn.user_prompt.is_empty() {
                                    turns.push(turn);
                                }
                            }

                            let message = payload.get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("")
                                .to_string();

                            current_turn = Some(Turn {
                                id: format!("turn-{}", turns.len()),
                                timestamp,
                                user_prompt: message,
                                thinking: None,
                                tool_invocations: Vec::new(),
                                response: String::new(),
                                model: default_model.map(String::from),
                            });
                        }
                        "agent_reasoning" => {
                            if let Some(turn) = current_turn.as_mut() {
                                let text = payload.get("text")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");
                                if !text.is_empty() {
                                    let existing = turn.thinking.take().unwrap_or_default();
                                    turn.thinking = Some(if existing.is_empty() {
                                        text.to_string()
                                    } else {
                                        format!("{}\n\n{}", existing, text)
                                    });
                                }
                            }
                        }
                        "agent_message" => {
                            if let Some(turn) = current_turn.as_mut() {
                                let message = payload.get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("");
                                if !message.is_empty() {
                                    if turn.response.is_empty() {
                                        turn.response = message.to_string();
                                    } else {
                                        turn.response.push_str("\n\n");
                                        turn.response.push_str(message);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "response_item" => {
                if let Some(payload) = entry.get("payload") {
                    let item_type = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match item_type {
                        "function_call" | "custom_tool_call" => {
                            let call_id = payload.get("call_id")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            pending_calls.insert(call_id, payload.clone());
                        }
                        "function_call_output" | "custom_tool_call_output" => {
                            let call_id = payload.get("call_id")
                                .and_then(|c| c.as_str())
                                .unwrap_or("");

                            if let Some(call) = pending_calls.remove(call_id) {
                                if let Some(turn) = current_turn.as_mut() {
                                    let invocation = create_tool_invocation(&call, payload);
                                    turn.tool_invocations.push(invocation);
                                }
                            }
                        }
                        "reasoning" => {
                            if let Some(turn) = current_turn.as_mut() {
                                let content = payload.get("content")
                                    .and_then(|c| c.as_array())
                                    .and_then(|arr| {
                                        arr.iter()
                                            .filter_map(|item| {
                                                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                                    item.get("text").and_then(|t| t.as_str())
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect::<Vec<_>>()
                                            .first()
                                            .copied()
                                    });
                                if let Some(text) = content {
                                    let existing = turn.thinking.take().unwrap_or_default();
                                    turn.thinking = Some(if existing.is_empty() {
                                        text.to_string()
                                    } else {
                                        format!("{}\n\n{}", existing, text)
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    // Finalize last turn
    if let Some(turn) = current_turn {
        if !turn.user_prompt.is_empty() {
            turns.push(turn);
        }
    }

    turns
}

/// Build turns from old JSON format items.
fn build_turns_from_json(items: &[Value]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut current_turn: Option<Turn> = None;
    let mut pending_calls: HashMap<String, Value> = HashMap::new();

    for item in items {
        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match item_type {
            "message" => {
                let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");
                let content = extract_message_content(item);

                match role {
                    "user" => {
                        // Finalize previous turn
                        if let Some(turn) = current_turn.take() {
                            if !turn.user_prompt.is_empty() {
                                turns.push(turn);
                            }
                        }

                        current_turn = Some(Turn {
                            id: format!("turn-{}", turns.len()),
                            timestamp: None,
                            user_prompt: content,
                            thinking: None,
                            tool_invocations: Vec::new(),
                            response: String::new(),
                            model: None,
                        });
                    }
                    "assistant" => {
                        if let Some(turn) = current_turn.as_mut() {
                            if turn.response.is_empty() {
                                turn.response = content;
                            } else {
                                turn.response.push_str("\n\n");
                                turn.response.push_str(&content);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "reasoning" => {
                if let Some(turn) = current_turn.as_mut() {
                    // Reasoning in old format might have summary array
                    let summary = item.get("summary")
                        .and_then(|s| s.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();

                    if !summary.is_empty() {
                        let existing = turn.thinking.take().unwrap_or_default();
                        turn.thinking = Some(if existing.is_empty() {
                            summary
                        } else {
                            format!("{}\n\n{}", existing, summary)
                        });
                    }
                }
            }
            "function_call" => {
                let call_id = item.get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                pending_calls.insert(call_id, item.clone());
            }
            "function_call_output" => {
                let call_id = item.get("call_id")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");

                if let Some(call) = pending_calls.remove(call_id) {
                    if let Some(turn) = current_turn.as_mut() {
                        let invocation = create_tool_invocation(&call, item);
                        turn.tool_invocations.push(invocation);
                    }
                }
            }
            _ => {}
        }
    }

    // Finalize last turn
    if let Some(turn) = current_turn {
        if !turn.user_prompt.is_empty() {
            turns.push(turn);
        }
    }

    turns
}

/// Extract text content from a message.
fn extract_message_content(item: &Value) -> String {
    if let Some(content) = item.get("content") {
        if let Some(s) = content.as_str() {
            return s.to_string();
        }
        if let Some(arr) = content.as_array() {
            return arr.iter()
                .filter_map(|c| {
                    let c_type = c.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if c_type == "input_text" || c_type == "text" {
                        c.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    String::new()
}

/// Create a tool invocation from call and output.
fn create_tool_invocation(call: &Value, output: &Value) -> ToolInvocation {
    let call_id = call.get("call_id")
        .or_else(|| call.get("id"))
        .and_then(|c| c.as_str())
        .unwrap_or("unknown")
        .to_string();

    let name = call.get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("unknown");

    let (tool_type, input_display, output_display) = parse_codex_tool(name, call, output);

    ToolInvocation {
        id: call_id,
        tool_type,
        input_display,
        output_display,
        raw_input: call.clone(),
        raw_output: Some(output.clone()),
    }
}

/// Parse Codex tool into our generic ToolType.
fn parse_codex_tool(name: &str, call: &Value, output: &Value) -> (ToolType, String, String) {
    match name {
        "shell_command" => {
            let args = parse_arguments(call);
            let command = args.get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let workdir = args.get("workdir")
                .and_then(|w| w.as_str())
                .map(String::from);

            let output_text = output.get("output")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();

            // Parse exit code from output
            let exit_code = output_text.lines()
                .find(|l| l.starts_with("Exit code:"))
                .and_then(|l| l.trim_start_matches("Exit code:").trim().parse().ok());

            // Extract actual output after headers
            let stdout = output_text.lines()
                .skip_while(|l| !l.starts_with("Output:"))
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n");

            let input_display = if let Some(wd) = &workdir {
                format!("[{}]\n$ {}", wd, command)
            } else {
                format!("$ {}", command)
            };

            (
                ToolType::Command {
                    command,
                    stdout: Some(stdout.clone()),
                    stderr: None,
                    exit_code,
                },
                input_display,
                truncate_display(&stdout, 500),
            )
        }
        "apply_patch" => {
            let input = call.get("input")
                .and_then(|i| i.as_str())
                .unwrap_or("");

            // Extract file path from patch
            let path = input.lines()
                .find(|l| l.starts_with("*** Update File:") || l.starts_with("*** Add File:"))
                .map(|l| l.trim_start_matches("*** Update File:")
                    .trim_start_matches("*** Add File:")
                    .trim()
                    .to_string())
                .unwrap_or_else(|| "unknown".to_string());

            let status = call.get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");

            (
                ToolType::FileEdit {
                    path: path.clone(),
                    old_content: None,
                    new_content: None,
                    diff: Some(input.to_string()),
                },
                truncate_display(&path, 60),
                format!("Patch {}", status),
            )
        }
        "update_plan" => {
            let args = parse_arguments(call);
            let plan = args.get("plan")
                .and_then(|p| p.as_str())
                .or_else(|| args.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");

            (
                ToolType::Other { name: "Plan".to_string() },
                truncate_display(plan, 200),
                "Plan updated".to_string(),
            )
        }
        _ => {
            let args = parse_arguments(call);
            let input_str = serde_json::to_string_pretty(&args).unwrap_or_default();
            let output_str = output.get("output")
                .and_then(|o| o.as_str())
                .unwrap_or("");

            (
                ToolType::Other { name: name.to_string() },
                truncate_display(&input_str, 200),
                truncate_display(output_str, 500),
            )
        }
    }
}

/// Parse arguments from call (handles string JSON in arguments field).
fn parse_arguments(call: &Value) -> Value {
    // Try arguments field (new format, JSON string)
    if let Some(args_str) = call.get("arguments").and_then(|a| a.as_str()) {
        if let Ok(args) = serde_json::from_str::<Value>(args_str) {
            return args;
        }
    }
    // Try input field (custom_tool_call)
    if let Some(input) = call.get("input") {
        if input.is_object() {
            return input.clone();
        }
    }
    // Return call itself as fallback
    call.clone()
}

/// Truncate a string for display, handling multi-byte UTF-8.
fn truncate_display(s: &str, max_chars: usize) -> String {
    let s = s.replace('\n', " ").replace('\r', "");
    if s.chars().count() > max_chars {
        format!("{}...", s.chars().take(max_chars.saturating_sub(3)).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ==================== SESSION LISTING TESTS ====================

    #[test]
    fn test_list_codex_sessions() {
        let sessions = list_codex_sessions();
        // Just verify it doesn't panic and returns a list
        // May be empty if no Codex sessions exist
        println!("Found {} Codex sessions", sessions.len());
    }

    // ==================== JSONL PARSING TESTS ====================

    #[test]
    fn test_build_turns_from_jsonl_user_message() {
        let entries = vec![
            json!({
                "timestamp": "2026-01-07T07:03:16.993Z",
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "Analyze the project",
                    "images": []
                }
            }),
            json!({
                "timestamp": "2026-01-07T07:05:37.141Z",
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": "Here is my analysis of the project structure."
                }
            }),
        ];

        let turns = build_turns_from_jsonl(&entries, Some("gpt-4"));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_prompt, "Analyze the project");
        assert!(turns[0].response.contains("analysis"));
        assert_eq!(turns[0].model, Some("gpt-4".to_string()));
    }

    #[test]
    fn test_build_turns_from_jsonl_with_reasoning() {
        let entries = vec![
            json!({
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "Help me debug this"
                }
            }),
            json!({
                "type": "event_msg",
                "payload": {
                    "type": "agent_reasoning",
                    "text": "**Analyzing the problem**\n\nLet me think about this step by step."
                }
            }),
            json!({
                "type": "event_msg",
                "payload": {
                    "type": "agent_reasoning",
                    "text": "The issue seems to be in the configuration."
                }
            }),
            json!({
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": "I found the issue."
                }
            }),
        ];

        let turns = build_turns_from_jsonl(&entries, None);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].thinking.is_some());
        let thinking = turns[0].thinking.as_ref().unwrap();
        assert!(thinking.contains("Analyzing the problem"));
        assert!(thinking.contains("configuration"));
    }

    #[test]
    fn test_build_turns_from_jsonl_with_tool_calls() {
        let entries = vec![
            json!({
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "List files"
                }
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "shell_command",
                    "arguments": "{\"command\":\"ls -la\",\"workdir\":\"/project\"}",
                    "call_id": "call_123"
                }
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_123",
                    "output": "Exit code: 0\nWall time: 0.1s\nOutput:\nfile1.txt\nfile2.txt"
                }
            }),
            json!({
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": "Found 2 files."
                }
            }),
        ];

        let turns = build_turns_from_jsonl(&entries, None);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].tool_invocations.len(), 1);

        let tool = &turns[0].tool_invocations[0];
        assert_eq!(tool.id, "call_123");
        assert!(matches!(&tool.tool_type, ToolType::Command { command, .. } if command == "ls -la"));
    }

    #[test]
    fn test_build_turns_from_jsonl_with_custom_tool() {
        let entries = vec![
            json!({
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "Add a new file"
                }
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": "call_456",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n+fn new_function() {}\n*** End Patch"
                }
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "call_id": "call_456",
                    "output": "Patch applied successfully"
                }
            }),
        ];

        let turns = build_turns_from_jsonl(&entries, None);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].tool_invocations.len(), 1);

        let tool = &turns[0].tool_invocations[0];
        assert!(matches!(&tool.tool_type, ToolType::FileEdit { path, diff, .. }
            if path == "src/main.rs" && diff.is_some()));
    }

    #[test]
    fn test_build_turns_from_jsonl_multiple_turns() {
        let entries = vec![
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "First question"}}),
            json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "First answer"}}),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "Second question"}}),
            json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "Second answer"}}),
        ];

        let turns = build_turns_from_jsonl(&entries, None);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user_prompt, "First question");
        assert_eq!(turns[1].user_prompt, "Second question");
    }

    // ==================== OLD JSON FORMAT TESTS ====================

    #[test]
    fn test_build_turns_from_json_basic() {
        let items = vec![
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Update the config"}]
            }),
            json!({
                "type": "reasoning",
                "id": "rs_123",
                "summary": ["Let me analyze the request", "I need to modify config.json"],
                "duration_ms": 1500
            }),
            json!({
                "type": "message",
                "role": "assistant",
                "content": "I've updated the configuration."
            }),
        ];

        let turns = build_turns_from_json(&items);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_prompt, "Update the config");
        assert!(turns[0].thinking.is_some());
        assert!(turns[0].thinking.as_ref().unwrap().contains("analyze"));
        assert!(turns[0].response.contains("updated"));
    }

    #[test]
    fn test_build_turns_from_json_with_function_calls() {
        let items = vec![
            json!({
                "type": "message",
                "role": "user",
                "content": "Run tests"
            }),
            json!({
                "type": "function_call",
                "id": "fc_001",
                "call_id": "fc_001",
                "name": "shell_command",
                "arguments": "{\"command\":\"npm test\"}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "fc_001",
                "output": "Exit code: 0\nOutput:\nAll tests passed"
            }),
            json!({
                "type": "message",
                "role": "assistant",
                "content": "All tests passed!"
            }),
        ];

        let turns = build_turns_from_json(&items);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].tool_invocations.len(), 1);
        assert!(matches!(&turns[0].tool_invocations[0].tool_type,
            ToolType::Command { command, .. } if command == "npm test"));
    }

    // ==================== TOOL PARSING TESTS ====================

    #[test]
    fn test_parse_shell_command() {
        let call = json!({
            "name": "shell_command",
            "arguments": "{\"command\":\"cargo build\",\"workdir\":\"/project\",\"timeout_ms\":60000}",
            "call_id": "call_789"
        });
        let output = json!({
            "output": "Exit code: 0\nWall time: 2.5s\nOutput:\n   Compiling project v0.1.0\n    Finished dev"
        });

        let (tool_type, input_display, _output_display) = parse_codex_tool("shell_command", &call, &output);

        if let ToolType::Command { command, stdout, exit_code, .. } = tool_type {
            assert_eq!(command, "cargo build");
            assert_eq!(exit_code, Some(0));
            assert!(stdout.unwrap().contains("Compiling"));
        } else {
            panic!("Expected Command tool type");
        }
        assert!(input_display.contains("cargo build"));
        assert!(input_display.contains("/project"));
    }

    #[test]
    fn test_parse_apply_patch() {
        let call = json!({
            "name": "apply_patch",
            "status": "completed",
            "call_id": "call_patch",
            "input": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n fn old() {}\n+fn new() {}\n*** End Patch"
        });
        let output = json!({"output": "Applied"});

        let (tool_type, input_display, _) = parse_codex_tool("apply_patch", &call, &output);

        if let ToolType::FileEdit { path, diff, .. } = tool_type {
            assert_eq!(path, "src/lib.rs");
            assert!(diff.is_some());
            assert!(diff.unwrap().contains("+fn new()"));
        } else {
            panic!("Expected FileEdit tool type");
        }
        assert!(input_display.contains("lib.rs"));
    }

    #[test]
    fn test_parse_update_plan() {
        let call = json!({
            "name": "update_plan",
            "arguments": "{\"plan\":\"1. Analyze codebase\\n2. Implement feature\\n3. Test\"}",
            "call_id": "call_plan"
        });
        let output = json!({"output": "Plan updated"});

        let (tool_type, input_display, _) = parse_codex_tool("update_plan", &call, &output);

        assert!(matches!(tool_type, ToolType::Other { name } if name == "Plan"));
        assert!(input_display.contains("Analyze"));
    }

    // ==================== UTILITY TESTS ====================

    #[test]
    fn test_truncate_display() {
        assert_eq!(truncate_display("short", 10), "short");
        assert_eq!(truncate_display("this is a long string", 10), "this is...");
        assert_eq!(truncate_display("line1\nline2\nline3", 20), "line1 line2 line3");
    }

    #[test]
    fn test_extract_message_content_string() {
        let item = json!({"content": "Simple string content"});
        assert_eq!(extract_message_content(&item), "Simple string content");
    }

    #[test]
    fn test_extract_message_content_array() {
        let item = json!({
            "content": [
                {"type": "input_text", "text": "First part"},
                {"type": "text", "text": "Second part"}
            ]
        });
        let content = extract_message_content(&item);
        assert!(content.contains("First part"));
        assert!(content.contains("Second part"));
    }

    #[test]
    fn test_parse_arguments_from_string() {
        let call = json!({
            "arguments": "{\"command\":\"ls\",\"workdir\":\"/tmp\"}"
        });
        let args = parse_arguments(&call);
        assert_eq!(args.get("command").and_then(|c| c.as_str()), Some("ls"));
        assert_eq!(args.get("workdir").and_then(|w| w.as_str()), Some("/tmp"));
    }

    // ==================== REAL SESSION INTEGRATION TESTS ====================

    #[test]
    fn test_parse_real_codex_jsonl_session() {
        let session_path = dirs::home_dir()
            .map(|h| h.join(".codex/sessions/2026/01/07/rollout-2026-01-07T12-02-49-019b9743-ed45-79e2-af72-3ce2c17f7413.jsonl"));

        if let Some(path) = session_path {
            if path.exists() {
                let session = parse_codex_session(&path).expect("Should parse session");

                assert!(!session.id.is_empty());
                assert!(!session.turns.is_empty());

                // Check that we have tool invocations
                let total_tools: usize = session.turns.iter()
                    .map(|t| t.tool_invocations.len())
                    .sum();
                assert!(total_tools > 0, "Expected tool invocations");

                // Check that we have thinking
                let has_thinking = session.turns.iter()
                    .any(|t| t.thinking.is_some());
                assert!(has_thinking, "Expected thinking/reasoning");

                println!("Parsed JSONL session: {} turns, {} tools",
                    session.turns.len(), total_tools);
            }
        }
    }

    #[test]
    fn test_parse_real_codex_json_session() {
        let session_path = dirs::home_dir()
            .map(|h| h.join(".codex/sessions/rollout-2025-04-17-0d977bc7-bc20-48e2-9b88-f3588e1ded60.json"));

        if let Some(path) = session_path {
            if path.exists() {
                let session = parse_codex_session(&path).expect("Should parse session");

                assert!(!session.id.is_empty());
                assert!(!session.turns.is_empty());

                println!("Parsed JSON session: {} turns", session.turns.len());
            }
        }
    }
}
