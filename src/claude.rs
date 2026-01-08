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
                .map(|t| {
                    let checkbox = match t.status.as_str() {
                        "completed" => "- [x]",
                        "in_progress" => "- [~]",
                        _ => "- [ ]", // pending or other
                    };
                    format!("{} {}", checkbox, t.content)
                })
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
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut result = Vec::new();

    for (idx, group) in diff.grouped_ops(3).iter().enumerate() {
        if idx > 0 {
            result.push(String::new());
        }

        for op in group {
            for change in diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                let value = change.value().trim_end_matches('\n');
                result.push(format!("{}{}", tag, value));
            }
        }
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

    // ==================== UTF-8 SAFETY TESTS ====================
    // Issue: "byte index 500 is not a char boundary" panic when truncating
    // multi-byte UTF-8 characters

    #[test]
    fn test_truncate_display_ascii() {
        let text = "Hello, World!";
        assert_eq!(truncate_display(text, 5), "Hello...");
        assert_eq!(truncate_display(text, 100), "Hello, World!");
    }

    #[test]
    fn test_truncate_display_multibyte_utf8() {
        // Japanese text - each character is 3 bytes
        let text = "こんにちは世界"; // "Hello World" in Japanese
        // Should not panic and should truncate by characters, not bytes
        let truncated = truncate_display(text, 3);
        assert_eq!(truncated, "こんに...");

        // Emoji - 4 bytes each
        let emoji_text = "🎉🎊🎁🎄🎅";
        let truncated_emoji = truncate_display(emoji_text, 2);
        assert_eq!(truncated_emoji, "🎉🎊...");
    }

    #[test]
    fn test_truncate_display_mixed_utf8() {
        // Mixed ASCII and multi-byte
        let text = "Hello 世界! 🌍";
        let truncated = truncate_display(text, 8);
        assert_eq!(truncated, "Hello 世界...");
    }

    // ==================== AGENT ID EXTRACTION TESTS ====================
    // Issue: agentId was including extra text like "(for resuming)"

    #[test]
    fn test_extract_agent_id_simple() {
        assert_eq!(extract_agent_id("agentId: abc123"), Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_agent_id_with_parenthetical() {
        // Real format from Claude Code sessions
        let text = "agentId: a0c970e (for resuming to continue this agent's work if needed)";
        assert_eq!(extract_agent_id(text), Some("a0c970e".to_string()));
    }

    #[test]
    fn test_extract_agent_id_multiline() {
        let text = "## Exploration Summary\n\nSome content here...\n\nagentId: a3dc895 (for resuming)";
        assert_eq!(extract_agent_id(text), Some("a3dc895".to_string()));
    }

    #[test]
    fn test_extract_agent_id_not_found() {
        assert_eq!(extract_agent_id("No agent ID here"), None);
        assert_eq!(extract_agent_id(""), None);
    }

    // ==================== TOOL USE RESULT EXTRACTION TESTS ====================
    // Issue: toolUseResult.content is an array of text blocks, not a string

    #[test]
    fn test_extract_result_string_from_string() {
        let result = json!("Simple string result");
        assert_eq!(
            extract_result_string(Some(&result), "tool1", None),
            Some("Simple string result".to_string())
        );
    }

    #[test]
    fn test_extract_result_string_from_content_string() {
        let result = json!({"content": "Content as string"});
        assert_eq!(
            extract_result_string(Some(&result), "tool1", None),
            Some("Content as string".to_string())
        );
    }

    #[test]
    fn test_extract_result_string_from_content_array() {
        // Real format from tool_result content
        let result = json!([
            {"type": "text", "text": "First part"},
            {"type": "text", "text": "Second part"}
        ]);
        let extracted = extract_result_string(Some(&result), "tool1", None);
        assert!(extracted.is_some());
        let text = extracted.unwrap();
        assert!(text.contains("First part"));
        assert!(text.contains("Second part"));
    }

    #[test]
    fn test_extract_result_string_from_tool_use_result_object() {
        // Real format from toolUseResult at entry level (Task tool)
        let result = json!({
            "status": "completed",
            "prompt": "Do something",
            "agentId": "a0c970e",
            "content": [
                {"type": "text", "text": "## Exploration Summary\n\nBased on my investigation..."},
                {"type": "text", "text": "agentId: a0c970e (for resuming)"}
            ],
            "totalDurationMs": 35224,
            "totalTokens": 18407
        });
        let extracted = extract_result_string(Some(&result), "tool1", None);
        assert!(extracted.is_some());
        let text = extracted.unwrap();
        assert!(text.contains("Exploration Summary"));
        assert!(text.contains("agentId: a0c970e"));
    }

    #[test]
    fn test_extract_result_string_from_bash_result() {
        // Bash tool result format
        let result = json!({
            "stdout": "file1.rs\nfile2.rs\nfile3.rs",
            "stderr": "",
            "interrupted": false,
            "isImage": false
        });
        assert_eq!(
            extract_result_string(Some(&result), "tool1", None),
            Some("file1.rs\nfile2.rs\nfile3.rs".to_string())
        );
    }

    #[test]
    fn test_extract_result_string_from_read_result() {
        // Read tool result format
        let result = json!({
            "type": "text",
            "file": {
                "filePath": "/path/to/file.rs",
                "content": "fn main() {\n    println!(\"Hello\");\n}",
                "numLines": 3,
                "startLine": 1,
                "totalLines": 3
            }
        });
        assert_eq!(
            extract_result_string(Some(&result), "tool1", None),
            Some("fn main() {\n    println!(\"Hello\");\n}".to_string())
        );
    }

    // ==================== TOOL TYPE PARSING TESTS ====================
    // Tests for various tool types with realistic inputs

    #[test]
    fn test_parse_read_tool() {
        let input = json!({"file_path": "/Users/test/project/Cargo.toml"});
        let result = json!({
            "type": "text",
            "file": {
                "filePath": "/Users/test/project/Cargo.toml",
                "content": "[package]\nname = \"test\"",
                "numLines": 2
            }
        });

        let (tool_type, input_display, _output_display) =
            parse_tool_type("Read", &input, Some(&result), "tool1", None, None);

        assert!(matches!(tool_type, ToolType::FileRead { path, content }
            if path == "/Users/test/project/Cargo.toml" && content.is_some()));
        assert_eq!(input_display, "/Users/test/project/Cargo.toml");
    }

    #[test]
    fn test_parse_edit_tool() {
        // Real Edit tool input format
        let input = json!({
            "file_path": "/Users/test/Cargo.toml",
            "old_string": "[dependencies]\nratatui = \"0.29\"",
            "new_string": "[dependencies]\nratatui = \"0.29\"\nserde = \"1.0\""
        });

        let (tool_type, _input_display, _output_display) =
            parse_tool_type("Edit", &input, None, "tool1", None, None);

        if let ToolType::FileEdit { path, old_content, new_content, diff } = tool_type {
            assert_eq!(path, "/Users/test/Cargo.toml");
            assert!(old_content.is_some());
            assert!(new_content.is_some());
            assert!(diff.is_some());
            // Diff should show the added line
            assert!(diff.unwrap().contains("+serde"));
        } else {
            panic!("Expected FileEdit tool type");
        }
    }

    #[test]
    fn test_parse_bash_tool() {
        let input = json!({
            "command": "cargo build",
            "description": "Build the project"
        });
        let result = json!({
            "stdout": "   Compiling test v0.1.0\n    Finished dev",
            "stderr": "",
            "interrupted": false
        });

        let (tool_type, input_display, output_display) =
            parse_tool_type("Bash", &input, Some(&result), "tool1", None, None);

        if let ToolType::Command { command, stdout, .. } = tool_type {
            assert_eq!(command, "cargo build");
            assert!(stdout.unwrap().contains("Compiling"));
        } else {
            panic!("Expected Command tool type");
        }
        assert!(input_display.contains("Build the project"));
        assert!(input_display.contains("$ cargo build"));
        assert!(output_display.contains("Compiling"));
    }

    #[test]
    fn test_parse_task_tool_with_agent_id() {
        // Real Task tool result format with agentId
        let input = json!({
            "subagent_type": "Explore",
            "prompt": "Explore the codebase",
            "description": "Explore codebase structure"
        });
        let result = json!({
            "status": "completed",
            "prompt": "Explore the codebase",
            "agentId": "a3dc895",
            "content": [
                {"type": "text", "text": "## Exploration Summary\n\nFound 10 files."},
                {"type": "text", "text": "agentId: a3dc895 (for resuming)"}
            ],
            "totalDurationMs": 35224
        });

        let (tool_type, _input_display, _output_display) =
            parse_tool_type("Task", &input, Some(&result), "tool1", None, None);

        if let ToolType::Task { description, prompt, subagent_type, result: result_str, .. } = tool_type {
            assert_eq!(description, "Explore codebase structure");
            assert_eq!(prompt, "Explore the codebase");
            assert_eq!(subagent_type, Some("Explore".to_string()));
            assert!(result_str.is_some());
            assert!(result_str.unwrap().contains("Exploration Summary"));
        } else {
            panic!("Expected Task tool type");
        }
    }

    #[test]
    fn test_parse_todo_write_tool() {
        let input = json!({
            "todos": [
                {"content": "Fix bug", "status": "completed", "activeForm": "Fixing bug"},
                {"content": "Add tests", "status": "in_progress", "activeForm": "Adding tests"},
                {"content": "Deploy", "status": "pending", "activeForm": "Deploying"}
            ]
        });

        let (tool_type, input_display, _) =
            parse_tool_type("TodoWrite", &input, None, "tool1", None, None);

        if let ToolType::TodoUpdate { todos } = tool_type {
            assert_eq!(todos.len(), 3);
            assert_eq!(todos[0].content, "Fix bug");
            assert_eq!(todos[0].status, "completed");
        } else {
            panic!("Expected TodoUpdate tool type");
        }
        assert!(input_display.contains("- [x] Fix bug"));
        assert!(input_display.contains("- [~] Add tests"));
        assert!(input_display.contains("- [ ] Deploy"));
    }

    #[test]
    fn test_parse_glob_tool() {
        let input = json!({"pattern": "**/*.rs", "path": "/project"});
        let result = json!([
            {"type": "text", "text": "src/main.rs\nsrc/lib.rs\nsrc/utils.rs"}
        ]);

        let (tool_type, input_display, _) =
            parse_tool_type("Glob", &input, Some(&result), "tool1", None, None);

        if let ToolType::Search { pattern, results } = tool_type {
            assert_eq!(pattern, "**/*.rs");
            assert_eq!(results.len(), 3);
            assert!(results.contains(&"src/main.rs".to_string()));
        } else {
            panic!("Expected Search tool type");
        }
        assert_eq!(input_display, "**/*.rs");
    }

    // ==================== TURN BUILDING TESTS ====================
    // Tests for building turns from session entries

    #[test]
    fn test_build_turns_user_assistant_pair() {
        let entries = vec![
            json!({
                "type": "user",
                "uuid": "user-1",
                "message": {
                    "role": "user",
                    "content": "Hello, help me with Rust"
                }
            }),
            json!({
                "type": "assistant",
                "uuid": "assistant-1",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-4-5-20251101",
                    "content": [
                        {"type": "text", "text": "I'd be happy to help with Rust!"}
                    ]
                }
            })
        ];

        let turns = build_turns(&entries, None, None);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_prompt, "Hello, help me with Rust");
        assert_eq!(turns[0].response, "I'd be happy to help with Rust!");
        assert_eq!(turns[0].model, Some("claude-opus-4-5-20251101".to_string()));
    }

    #[test]
    fn test_build_turns_with_thinking() {
        let entries = vec![
            json!({
                "type": "user",
                "uuid": "user-1",
                "message": {"role": "user", "content": "Explain closures"}
            }),
            json!({
                "type": "assistant",
                "uuid": "assistant-1",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Let me think about how to explain closures clearly..."},
                        {"type": "text", "text": "Closures are anonymous functions that capture their environment."}
                    ]
                }
            })
        ];

        let turns = build_turns(&entries, None, None);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].thinking.is_some());
        assert!(turns[0].thinking.as_ref().unwrap().contains("think about"));
        assert!(turns[0].response.contains("Closures are"));
    }

    #[test]
    fn test_build_turns_with_tool_use_and_result() {
        let entries = vec![
            json!({
                "type": "user",
                "uuid": "user-1",
                "message": {"role": "user", "content": "Read my Cargo.toml"}
            }),
            json!({
                "type": "assistant",
                "uuid": "assistant-1",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Let me read that file."},
                        {
                            "type": "tool_use",
                            "id": "toolu_123",
                            "name": "Read",
                            "input": {"file_path": "/project/Cargo.toml"}
                        }
                    ]
                }
            }),
            json!({
                "type": "user",
                "uuid": "user-2",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_123",
                            "content": "[package]\nname = \"myproject\""
                        }
                    ]
                }
            }),
            json!({
                "type": "assistant",
                "uuid": "assistant-2",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Your project is named 'myproject'."}
                    ]
                }
            })
        ];

        let turns = build_turns(&entries, None, None);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].tool_invocations.len(), 1);

        let tool = &turns[0].tool_invocations[0];
        assert_eq!(tool.id, "toolu_123");
        assert!(matches!(&tool.tool_type, ToolType::FileRead { path, .. } if path == "/project/Cargo.toml"));
    }

    #[test]
    fn test_build_turns_multiple_turns() {
        let entries = vec![
            json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "First question"}}),
            json!({"type": "assistant", "uuid": "a1", "message": {"role": "assistant", "content": [{"type": "text", "text": "First answer"}]}}),
            json!({"type": "user", "uuid": "u2", "message": {"role": "user", "content": "Second question"}}),
            json!({"type": "assistant", "uuid": "a2", "message": {"role": "assistant", "content": [{"type": "text", "text": "Second answer"}]}}),
        ];

        let turns = build_turns(&entries, None, None);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user_prompt, "First question");
        assert_eq!(turns[1].user_prompt, "Second question");
    }

    // ==================== ENTRY-LEVEL toolUseResult TESTS ====================
    // Issue: agentId is in entry.toolUseResult, not in message.content[].content

    #[test]
    fn test_process_tool_results_uses_entry_tool_use_result() {
        // This tests that process_tool_results prefers entry-level toolUseResult
        // which contains the agentId for Task tools
        let entry = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_task_123",
                        "content": [
                            {"type": "text", "text": "Result text"},
                            {"type": "text", "text": "agentId: abc123 (for resuming)"}
                        ]
                    }
                ]
            },
            "toolUseResult": {
                "status": "completed",
                "agentId": "abc123",
                "content": [
                    {"type": "text", "text": "Result from toolUseResult"},
                    {"type": "text", "text": "agentId: abc123 (for resuming)"}
                ]
            }
        });

        let tool_use = json!({
            "type": "tool_use",
            "id": "toolu_task_123",
            "name": "Task",
            "input": {
                "description": "Test task",
                "prompt": "Do something",
                "subagent_type": "Explore"
            }
        });

        let mut pending = HashMap::new();
        pending.insert("toolu_task_123".to_string(), tool_use);
        let mut invocations = Vec::new();

        process_tool_results(&entry, &mut pending, &mut invocations, None, None);

        assert_eq!(invocations.len(), 1);
        // The result should come from toolUseResult which has the agentId
        if let ToolType::Task { result, .. } = &invocations[0].tool_type {
            assert!(result.is_some());
            // Should contain content from toolUseResult
            assert!(result.as_ref().unwrap().contains("Result from toolUseResult"));
        } else {
            panic!("Expected Task tool type");
        }
    }

    // ==================== REAL SESSION INTEGRATION TEST ====================

    #[test]
    fn test_parse_real_session_with_subagents() {
        // Test parsing real session and verify Task tools load subagent turns
        let session_path = dirs::home_dir()
            .map(|h| h.join(".claude/projects/-Users-dzhanguzin-dev-promptui/063cd168-91d2-41bd-b7ba-5d2dee7fc7ab.jsonl"));

        if let Some(path) = session_path {
            if path.exists() {
                let session = parse_session(&path).expect("Should parse session");

                // Verify session metadata
                assert!(!session.id.is_empty());
                assert!(matches!(session.source, SessionSource::ClaudeCode { .. }));

                // Count tools by type
                let mut task_count = 0;
                let mut task_with_subagent_count = 0;
                let mut read_count = 0;
                let mut edit_count = 0;
                let mut _bash_count = 0;

                for turn in &session.turns {
                    for tool in &turn.tool_invocations {
                        match &tool.tool_type {
                            ToolType::Task { subagent_turns, .. } => {
                                task_count += 1;
                                if !subagent_turns.is_empty() {
                                    task_with_subagent_count += 1;
                                }
                            }
                            ToolType::FileRead { .. } => read_count += 1,
                            ToolType::FileEdit { .. } => edit_count += 1,
                            ToolType::Command { .. } => _bash_count += 1,
                            _ => {}
                        }
                    }
                }

                // Based on the session we explored, we expect:
                // - 2 Task tools
                // - Multiple Read, Edit, Bash tools
                assert!(task_count >= 2, "Expected at least 2 Task tools, got {}", task_count);
                assert!(task_with_subagent_count >= 1,
                    "Expected at least 1 Task with subagent turns, got {}", task_with_subagent_count);
                assert!(read_count > 0, "Expected Read tools");
                assert!(edit_count > 0, "Expected Edit tools");
            }
        }
    }

    #[test]
    fn test_list_sessions_excludes_agent_files() {
        let project_path = dirs::home_dir()
            .map(|h| h.join(".claude/projects/-Users-dzhanguzin-dev-promptui"));

        if let Some(path) = project_path {
            if path.exists() {
                let sessions = list_sessions(&path);

                // Verify no agent-* files are included
                for session in &sessions {
                    assert!(
                        !session.name.starts_with("agent-"),
                        "Session list should not include agent files: {}",
                        session.name
                    );
                }
            }
        }
    }

    // ==================== EDGE CASES ====================

    #[test]
    fn test_empty_entries() {
        let entries: Vec<Value> = vec![];
        let turns = build_turns(&entries, None, None);
        assert!(turns.is_empty());
    }

    #[test]
    fn test_user_message_without_assistant() {
        let entries = vec![
            json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "Hello"}}),
        ];
        let turns = build_turns(&entries, None, None);
        // Should create a turn even without assistant response
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_prompt, "Hello");
        assert!(turns[0].response.is_empty());
    }

    #[test]
    fn test_user_message_with_array_content() {
        // User message with content as array (includes input_text type)
        let entries = vec![
            json!({
                "type": "user",
                "uuid": "u1",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "First part"},
                        {"type": "text", "text": "Second part"}
                    ]
                }
            }),
            json!({
                "type": "assistant",
                "uuid": "a1",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "Response"}]}
            }),
        ];

        let turns = build_turns(&entries, None, None);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].user_prompt.contains("First part"));
    }

    #[test]
    fn test_skip_non_message_entries() {
        let entries = vec![
            json!({"type": "file-history-snapshot", "messageId": "123"}),
            json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "Hello"}}),
            json!({"type": "assistant", "uuid": "a1", "message": {"role": "assistant", "content": [{"type": "text", "text": "Hi"}]}}),
        ];

        let turns = build_turns(&entries, None, None);
        assert_eq!(turns.len(), 1);
    }
}
