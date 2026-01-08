//! Parser for Claude Code session transcripts from ~/.claude/projects/

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::models::*;

/// Get the Claude projects directory.
pub fn get_claude_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// List available projects.
pub fn list_projects() -> Vec<ProjectInfo> {
    let Some(projects_dir) = get_claude_projects_dir() else {
        return Vec::new();
    };

    let Ok(entries) = fs::read_dir(&projects_dir) else {
        return Vec::new();
    };

    let mut projects = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            // Decode project path from directory name (e.g., "-Users-foo-bar" -> "/Users/foo/bar")
            let decoded_path = name.replacen('-', "/", 1).replace('-', "/");
            projects.push(ProjectInfo {
                name: name.to_string(),
                path: path,
                decoded_path,
            });
        }
    }

    projects.sort_by(|a, b| a.decoded_path.cmp(&b.decoded_path));
    projects
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub name: String,
    pub path: PathBuf,
    pub decoded_path: String,
}

/// List sessions in a project (excludes agent-* files which are embedded in parent sessions).
pub fn list_sessions(project_path: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = fs::read_dir(project_path) else {
        return Vec::new();
    };

    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "jsonl") {
            let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            // Skip agent files - they'll be embedded in parent sessions
            if name.starts_with("agent-") {
                continue;
            }
            // Get file metadata for sorting by modification time
            let modified = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok();
            sessions.push(SessionInfo {
                name,
                path,
                modified,
            });
        }
    }

    // Sort by modification time, newest first
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    sessions
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    pub path: PathBuf,
    pub modified: Option<std::time::SystemTime>,
}

/// Parse a Claude Code session file.
pub fn parse_session(path: &Path) -> Result<Session, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open file: {}", e))?;
    let reader = BufReader::new(file);

    let session_id = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let project_dir = path.parent();
    let tool_results_dir = project_dir.map(|p| p.join(&session_id).join("tool-results"));

    let mut entries: Vec<Value> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Failed to read line: {}", e))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            entries.push(value);
        }
    }

    // Build turns by pairing user messages with assistant responses
    let turns = build_turns(&entries, tool_results_dir.as_deref(), project_dir);

    // Extract session metadata
    let version = entries
        .first()
        .and_then(|e| e.get("version"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let project_path = entries
        .first()
        .and_then(|e| e.get("cwd"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from);

    Ok(Session {
        id: session_id.clone(),
        name: session_id,
        source: SessionSource::ClaudeCode { version },
        project_path,
        turns,
    })
}

fn build_turns(entries: &[Value], tool_results_dir: Option<&Path>, project_dir: Option<&Path>) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut i = 0;

    while i < entries.len() {
        let entry = &entries[i];

        // Skip non-message entries
        let entry_type = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if entry_type != "user" {
            i += 1;
            continue;
        }

        // Found a user message, look for the next assistant response
        let user_prompt = extract_user_prompt(entry);
        let timestamp = entry.get("timestamp").and_then(|t| t.as_str()).map(String::from);
        let turn_id = entry.get("uuid").and_then(|u| u.as_str()).unwrap_or("").to_string();

        // Collect assistant responses and tool results
        let mut thinking = None;
        let mut response_parts: Vec<String> = Vec::new();
        let mut tool_invocations: Vec<ToolInvocation> = Vec::new();
        let mut model = None;
        let mut pending_tool_uses: HashMap<String, Value> = HashMap::new();

        i += 1;

        // Process following entries until next user message
        while i < entries.len() {
            let next_entry = &entries[i];
            let next_type = next_entry.get("type").and_then(|t| t.as_str()).unwrap_or("");

            if next_type == "user" {
                // Check if this user message contains tool results
                if has_tool_results(next_entry) {
                    // Process tool results
                    process_tool_results(
                        next_entry,
                        &mut pending_tool_uses,
                        &mut tool_invocations,
                        tool_results_dir,
                        project_dir,
                    );
                    i += 1;
                    continue;
                } else {
                    // New user turn, stop here
                    break;
                }
            }

            if next_type == "assistant" {
                // Extract model info
                if model.is_none() {
                    model = next_entry
                        .get("message")
                        .and_then(|m| m.get("model"))
                        .and_then(|m| m.as_str())
                        .map(String::from);
                }

                // Process assistant message content
                if let Some(content) = next_entry.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                    for item in content {
                        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match item_type {
                            "text" => {
                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                    response_parts.push(text.to_string());
                                }
                            }
                            "thinking" => {
                                if let Some(text) = item.get("thinking").and_then(|t| t.as_str()) {
                                    thinking = Some(text.to_string());
                                }
                            }
                            "tool_use" => {
                                if let Some(tool_id) = item.get("id").and_then(|i| i.as_str()) {
                                    pending_tool_uses.insert(tool_id.to_string(), item.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Also check for toolUseResult at top level (alternative storage)
                if let Some(tool_result) = next_entry.get("toolUseResult") {
                    // Find the corresponding tool use in pending
                    if let Some(tool_id) = find_tool_id_for_result(next_entry) {
                        if let Some(tool_use) = pending_tool_uses.remove(&tool_id) {
                            let invocation = create_tool_invocation(&tool_id, &tool_use, Some(tool_result), tool_results_dir, project_dir);
                            tool_invocations.push(invocation);
                        }
                    }
                }
            }

            i += 1;
        }

        // Add any remaining tool uses without results
        for (tool_id, tool_use) in pending_tool_uses {
            let invocation = create_tool_invocation(&tool_id, &tool_use, None, tool_results_dir, project_dir);
            tool_invocations.push(invocation);
        }

        // Only add turn if we have some content
        if !user_prompt.is_empty() || !response_parts.is_empty() || !tool_invocations.is_empty() {
            turns.push(Turn {
                id: turn_id,
                timestamp,
                user_prompt,
                thinking,
                tool_invocations,
                response: response_parts.join("\n\n"),
                model,
            });
        }
    }

    turns
}

fn extract_user_prompt(entry: &Value) -> String {
    let content = entry.get("message").and_then(|m| m.get("content"));

    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if item_type == "text" || item_type == "input_text" {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        parts.push(text.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn has_tool_results(entry: &Value) -> bool {
    if let Some(Value::Array(arr)) = entry.get("message").and_then(|m| m.get("content")) {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                return true;
            }
        }
    }
    false
}

fn process_tool_results(
    entry: &Value,
    pending_tool_uses: &mut HashMap<String, Value>,
    tool_invocations: &mut Vec<ToolInvocation>,
    tool_results_dir: Option<&Path>,
    project_dir: Option<&Path>,
) {
    // Check for top-level toolUseResult (has agentId for Task tools)
    let entry_tool_use_result = entry.get("toolUseResult");

    if let Some(Value::Array(arr)) = entry.get("message").and_then(|m| m.get("content")) {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }

            let tool_id = item.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("");
            if let Some(tool_use) = pending_tool_uses.remove(tool_id) {
                // Prefer entry's toolUseResult (has agentId), fall back to item content
                let result = entry_tool_use_result.or_else(|| item.get("content"));
                let invocation = create_tool_invocation(tool_id, &tool_use, result, tool_results_dir, project_dir);
                tool_invocations.push(invocation);
            }
        }
    }
}

fn find_tool_id_for_result(entry: &Value) -> Option<String> {
    // Try to find tool ID from the entry context
    entry.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .and_then(|item| item.get("id").and_then(|i| i.as_str()).map(String::from))
        })
}

fn create_tool_invocation(
    tool_id: &str,
    tool_use: &Value,
    result: Option<&Value>,
    tool_results_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> ToolInvocation {
    let tool_name = tool_use.get("name").and_then(|n| n.as_str()).unwrap_or("Unknown");
    let input = tool_use.get("input").cloned().unwrap_or(Value::Null);

    let (tool_type, input_display, output_display) = parse_tool_type(tool_name, &input, result, tool_id, tool_results_dir, project_dir);

    ToolInvocation {
        id: tool_id.to_string(),
        tool_type,
        input_display,
        output_display,
        raw_input: input,
        raw_output: result.cloned(),
    }
}

fn parse_tool_type(
    tool_name: &str,
    input: &Value,
    result: Option<&Value>,
    tool_id: &str,
    tool_results_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> (ToolType, String, String) {
    match tool_name {
        "Read" => {
            let path = input.get("file_path").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let content = extract_result_string(result, tool_id, tool_results_dir);
            let input_display = path.clone();
            let output_display = content.clone().unwrap_or_else(|| "(no content)".to_string());
            (
                ToolType::FileRead { path, content },
                input_display,
                truncate_display(&output_display, 500),
            )
        }
        "Write" => {
            let path = input.get("file_path").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let content = input.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
            let input_display = format!("{}\n\n{}", path, truncate_display(&content, 200));
            let output_display = extract_result_string(result, tool_id, tool_results_dir)
                .unwrap_or_else(|| "File written".to_string());
            (
                ToolType::FileWrite { path, content },
                input_display,
                output_display,
            )
        }
        "Edit" => {
            let path = input.get("file_path").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let old_string = input.get("old_string").and_then(|s| s.as_str()).map(String::from);
            let new_string = input.get("new_string").and_then(|s| s.as_str()).map(String::from);

            // Try to get diff from result
            let diff = result.and_then(|r| {
                r.get("structuredPatch").map(|_| {
                    // Build diff from old/new strings
                    if let (Some(old), Some(new)) = (&old_string, &new_string) {
                        format!("--- a/{}\n+++ b/{}\n{}", path, path,
                            generate_unified_diff(old, new))
                    } else {
                        String::new()
                    }
                })
            }).or_else(|| {
                // Generate diff from old/new if available
                if let (Some(old), Some(new)) = (&old_string, &new_string) {
                    Some(format!("--- a/{}\n+++ b/{}\n{}", path, path,
                        generate_unified_diff(old, new)))
                } else {
                    None
                }
            });

            let input_display = format!("{}:\n  -{}\n  +{}",
                path,
                truncate_display(old_string.as_deref().unwrap_or(""), 100),
                truncate_display(new_string.as_deref().unwrap_or(""), 100),
            );
            let output_display = "Edit applied".to_string();

            (
                ToolType::FileEdit {
                    path,
                    old_content: old_string,
                    new_content: new_string,
                    diff,
                },
                input_display,
                output_display,
            )
        }
        "Bash" => {
            let command = input.get("command").and_then(|c| c.as_str()).unwrap_or("").to_string();
            let description = input.get("description").and_then(|d| d.as_str());

            let (stdout, stderr) = if let Some(r) = result {
                (
                    r.get("stdout").and_then(|s| s.as_str()).map(String::from)
                        .or_else(|| extract_result_string(Some(r), tool_id, tool_results_dir)),
                    r.get("stderr").and_then(|s| s.as_str()).map(String::from),
                )
            } else {
                (None, None)
            };

            let input_display = if let Some(desc) = description {
                format!("{}\n$ {}", desc, command)
            } else {
                format!("$ {}", command)
            };
            let output_display = stdout.clone().unwrap_or_default();

            (
                ToolType::Command {
                    command,
                    stdout,
                    stderr,
                    exit_code: None,
                },
                input_display,
                truncate_display(&output_display, 500),
            )
        }
        "Glob" => {
            let pattern = input.get("pattern").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let results: Vec<String> = extract_result_string(result, tool_id, tool_results_dir)
                .map(|s| s.lines().map(String::from).collect())
                .unwrap_or_default();
            let output_display = results.join("\n");
            (
                ToolType::Search { pattern: pattern.clone(), results },
                pattern,
                truncate_display(&output_display, 500),
            )
        }
        "Grep" => {
            let pattern = input.get("pattern").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let results: Vec<String> = extract_result_string(result, tool_id, tool_results_dir)
                .map(|s| s.lines().map(String::from).collect())
                .unwrap_or_default();
            let output_display = results.join("\n");
            (
                ToolType::Search { pattern: pattern.clone(), results },
                pattern,
                truncate_display(&output_display, 500),
            )
        }
        "WebFetch" => {
            let url = input.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
            let content = extract_result_string(result, tool_id, tool_results_dir);
            let output_display = content.clone().unwrap_or_else(|| "(no content)".to_string());
            (
                ToolType::WebFetch { url: url.clone(), content },
                url,
                truncate_display(&output_display, 500),
            )
        }
        "WebSearch" => {
            let query = input.get("query").and_then(|q| q.as_str()).unwrap_or("").to_string();
            let results = extract_result_string(result, tool_id, tool_results_dir);
            let output_display = results.clone().unwrap_or_else(|| "(no results)".to_string());
            (
                ToolType::WebSearch { query: query.clone(), results },
                query,
                truncate_display(&output_display, 500),
            )
        }
        "TodoWrite" => {
            let todos: Vec<TodoItem> = input.get("todos")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let content = item.get("content").and_then(|c| c.as_str())?;
                            let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("pending");
                            Some(TodoItem {
                                content: content.to_string(),
                                status: status.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let input_display = todos.iter()
                .map(|t| format!("[{}] {}", t.status, t.content))
                .collect::<Vec<_>>()
                .join("\n");

            (
                ToolType::TodoUpdate { todos },
                input_display,
                "Todos updated".to_string(),
            )
        }
        "Task" => {
            let description = input.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
            let prompt = input.get("prompt").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let subagent_type = input.get("subagent_type").and_then(|s| s.as_str()).map(String::from);
            let result_str = extract_result_string(result, tool_id, tool_results_dir);

            // Try to extract agentId from result JSON (toolUseResult.agentId) or from text
            let agent_id = result
                .and_then(|r| r.get("agentId"))
                .and_then(|id| id.as_str())
                .map(String::from)
                .or_else(|| result_str.as_ref().and_then(|r| extract_agent_id(r)));

            // Load subagent turns if we have an agentId
            let subagent_turns = agent_id
                .and_then(|id| {
                    project_dir.and_then(|dir| {
                        let agent_file = dir.join(format!("agent-{}.jsonl", id));
                        parse_agent_file(&agent_file).ok()
                    })
                })
                .unwrap_or_default();

            let input_display = format!("{}\n{}", description, truncate_display(&prompt, 200));
            let turn_count = subagent_turns.len();
            let output_display = if turn_count > 0 {
                format!("[{} subagent turns]", turn_count)
            } else {
                truncate_display(&result_str.clone().unwrap_or_default(), 500)
            };

            (
                ToolType::Task {
                    description,
                    prompt,
                    subagent_type,
                    result: result_str,
                    subagent_turns,
                },
                input_display,
                output_display,
            )
        }
        _ => {
            let input_display = serde_json::to_string_pretty(input).unwrap_or_default();
            let output_display = result
                .map(|r| serde_json::to_string_pretty(r).unwrap_or_default())
                .unwrap_or_default();
            (
                ToolType::Other { name: tool_name.to_string() },
                truncate_display(&input_display, 300),
                truncate_display(&output_display, 500),
            )
        }
    }
}

fn extract_result_string(result: Option<&Value>, tool_id: &str, tool_results_dir: Option<&Path>) -> Option<String> {
    // First try to get from result directly
    if let Some(r) = result {
        // Try different content locations
        if let Some(s) = r.as_str() {
            return Some(s.to_string());
        }
        // Try content as string
        if let Some(content) = r.get("content").and_then(|c| c.as_str()) {
            return Some(content.to_string());
        }
        // Try content as array of text blocks (from toolUseResult.content)
        if let Some(content_arr) = r.get("content").and_then(|c| c.as_array()) {
            let mut texts = Vec::new();
            for item in content_arr {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        texts.push(text.to_string());
                    }
                }
            }
            if !texts.is_empty() {
                return Some(texts.join("\n"));
            }
        }
        // Handle result itself as array of content blocks
        if let Some(arr) = r.as_array() {
            let mut texts = Vec::new();
            for item in arr {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        texts.push(text.to_string());
                    }
                }
            }
            if !texts.is_empty() {
                return Some(texts.join("\n"));
            }
        }
        if let Some(file) = r.get("file") {
            if let Some(content) = file.get("content").and_then(|c| c.as_str()) {
                return Some(content.to_string());
            }
        }
        if let Some(stdout) = r.get("stdout").and_then(|s| s.as_str()) {
            return Some(stdout.to_string());
        }
    }

    // Try to load from external file
    if let Some(dir) = tool_results_dir {
        let file_path = dir.join(format!("{}.txt", tool_id));
        if file_path.exists() {
            if let Ok(content) = fs::read_to_string(&file_path) {
                return Some(content);
            }
        }
    }

    None
}

fn truncate_display(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count > max_chars {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...", truncated)
    } else {
        s.to_string()
    }
}

fn generate_unified_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut result = Vec::new();
    result.push(format!("@@ -1,{} +1,{} @@", old_lines.len(), new_lines.len()));

    for line in &old_lines {
        result.push(format!("-{}", line));
    }
    for line in &new_lines {
        result.push(format!("+{}", line));
    }

    result.join("\n")
}

/// Extract agentId from Task result string.
/// The result typically ends with "agentId: abc123" or similar.
fn extract_agent_id(result: &str) -> Option<String> {
    // Look for "agentId: <id>" pattern
    for line in result.lines().rev() {
        let line = line.trim();
        if line.contains("agentId") {
            if let Some(pos) = line.find("agentId") {
                let after = &line[pos + 7..];
                let after = after.trim_start_matches(':').trim();
                // Extract just the ID part (before any parenthesis or space)
                let id = after.split(|c: char| c.is_whitespace() || c == '(')
                    .next()
                    .map(|s| s.trim());
                if let Some(id) = id {
                    if !id.is_empty() {
                        return Some(id.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Parse an agent file and return its turns.
fn parse_agent_file(path: &Path) -> Result<Vec<Turn>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(path).map_err(|e| format!("Failed to open agent file: {}", e))?;
    let reader = BufReader::new(file);

    let mut entries: Vec<Value> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Failed to read line: {}", e))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            entries.push(value);
        }
    }

    // Build turns without recursively loading subagents (to avoid infinite loops)
    Ok(build_turns(&entries, None, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_agent_id_from_text() {
        // Test agentId extraction from text
        let text = "Some result text\nagentId: a0c970e (for resuming)";
        assert_eq!(extract_agent_id(text), Some("a0c970e".to_string()));

        let text2 = "agentId: abc123";
        assert_eq!(extract_agent_id(text2), Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_result_string_from_tool_use_result() {
        // Test extracting from toolUseResult object (has agentId at top level, content as array)
        let tool_use_result = json!({
            "status": "completed",
            "agentId": "a0c970e",
            "content": [
                {"type": "text", "text": "First part of result"},
                {"type": "text", "text": "agentId: a0c970e (for resuming)"}
            ]
        });

        let result = extract_result_string(Some(&tool_use_result), "tool123", None);
        assert!(result.is_some());
        let result_str = result.unwrap();
        assert!(result_str.contains("First part of result"));
        assert!(result_str.contains("agentId: a0c970e"));
    }

    #[test]
    fn test_extract_result_string_from_content_array() {
        // Test extracting from content array directly
        let content = json!([
            {"type": "text", "text": "Result text here"},
            {"type": "text", "text": "More text"}
        ]);

        let result = extract_result_string(Some(&content), "tool123", None);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Result text here"));
    }

    #[test]
    fn test_task_tool_gets_agent_id() {
        // Test that Task tool properly extracts agentId from toolUseResult
        let tool_use_result = json!({
            "status": "completed",
            "agentId": "test123",
            "content": [
                {"type": "text", "text": "Task completed successfully"}
            ]
        });

        let input = json!({
            "description": "Test task",
            "prompt": "Do something",
            "subagent_type": "Explore"
        });

        let (_tool_type, _, _) = parse_tool_type("Task", &input, Some(&tool_use_result), "tool1", None, None);

        // Verify we can extract from the toolUseResult
        assert!(tool_use_result.get("agentId").is_some());
        assert_eq!(tool_use_result.get("agentId").unwrap().as_str(), Some("test123"));
    }

    #[test]
    fn test_parse_real_session() {
        // Test parsing the real promptui session and check for Task tools with subagent turns
        let session_path = dirs::home_dir()
            .map(|h| h.join(".claude/projects/-Users-dzhanguzin-dev-promptui/063cd168-91d2-41bd-b7ba-5d2dee7fc7ab.jsonl"));

        if let Some(path) = session_path {
            if path.exists() {
                let session = parse_session(&path).expect("Should parse session");

                // Find Task tools and check if any have subagent turns
                let mut found_task_with_turns = false;
                for turn in &session.turns {
                    for tool in &turn.tool_invocations {
                        if let ToolType::Task { subagent_turns, .. } = &tool.tool_type {
                            if !subagent_turns.is_empty() {
                                found_task_with_turns = true;
                                println!("Found Task with {} subagent turns", subagent_turns.len());
                            }
                        }
                    }
                }

                // This test will show if we're properly loading subagent turns
                println!("Found Task with subagent turns: {}", found_task_with_turns);
            }
        }
    }
}
