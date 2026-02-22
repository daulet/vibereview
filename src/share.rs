//! Session sharing functionality: export, compression, upload, and clipboard operations.

use crate::models::{Session, ToolInvocation, ToolType, Turn};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use arboard::Clipboard;
use base64::{
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD},
    Engine as _,
};
use chrono::Utc;
use color_eyre::Result;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Environment variable for the cloud share API endpoint.
pub const SHARE_API_URL_ENV: &str = "VIBEREVIEW_SHARE_API_URL";
pub const SHARE_KEY_PARAM: &str = "k";
const CLOUD_SHARE_KEY_LEN: usize = 32;
const CLOUD_SHARE_NONCE_LEN: usize = 12;
const CLOUD_SHARE_MAGIC: &[u8; 4] = b"VRE1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShareExportMode {
    PromptResponseOnly,
    PromptResponseAndDiff,
    FullSession,
}

impl ShareExportMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PromptResponseOnly => "Prompt + Response only",
            Self::PromptResponseAndDiff => "Prompt + Response + Diff",
            Self::FullSession => "Full session",
        }
    }

    #[must_use]
    pub const fn is_resumable(self) -> bool {
        matches!(self, Self::FullSession)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeSource {
    Claude,
    Codex,
}

#[derive(Debug, Clone)]
pub struct ResumeBundleInput {
    pub source: ResumeSource,
    pub resume_session_id: String,
    pub resume_command: String,
    pub project_path_hint: PathBuf,
    pub session_paths: Vec<PathBuf>,
}

/// A shared session with metadata for the web viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SharedSession {
    /// Schema version for future compatibility
    pub version: u8,
    /// The session data
    pub session: Session,
    /// ISO 8601 timestamp of when the session was shared
    pub shared_at: String,
}

impl SharedSession {
    #[allow(dead_code)]
    pub fn new(session: Session) -> Self {
        Self {
            version: 1,
            session,
            shared_at: Utc::now().to_rfc3339(),
        }
    }
}

/// Portable single-file export format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortableSharedFile {
    pub version: u8,
    pub shared_at: String,
    pub mode: ShareExportMode,
    pub session: Session,
    /// Present only for `FullSession` exports.
    pub resume_bundle: Option<ResumeBundle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeBundle {
    pub source: ResumeSource,
    pub resume_session_id: String,
    pub resume_command: String,
    pub project_path_hint: String,
    pub artifacts: Vec<ResumeArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeArtifact {
    /// Restore path relative to home directory (e.g. ".claude/projects/...").
    pub restore_path: String,
    /// Original absolute path on the source machine.
    pub source_path: String,
    /// File content encoded as base64.
    pub content_base64: String,
    pub size_bytes: usize,
}

/// Response from the upload API
#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    #[allow(dead_code)]
    pub id: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct CloudShareLocator {
    pub id: String,
    pub api_url: String,
    pub key: Option<Vec<u8>>,
}

/// Returns configured cloud share API URL if available.
#[must_use]
pub fn cloud_share_api_url() -> Option<String> {
    let value = env::var(SHARE_API_URL_ENV).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[must_use]
pub fn generate_cloud_share_key() -> [u8; CLOUD_SHARE_KEY_LEN] {
    let mut key = [0_u8; CLOUD_SHARE_KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

#[must_use]
pub fn encode_cloud_share_key(key: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(key)
}

pub fn decode_cloud_share_key(encoded: &str) -> Result<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD.decode(encoded.trim())?;
    if decoded.len() != CLOUD_SHARE_KEY_LEN {
        return Err(color_eyre::eyre::eyre!(
            "Invalid share key length: expected {} bytes, got {}",
            CLOUD_SHARE_KEY_LEN,
            decoded.len()
        ));
    }
    Ok(decoded)
}

#[must_use]
pub fn is_encrypted_cloud_payload(payload: &[u8]) -> bool {
    payload.starts_with(CLOUD_SHARE_MAGIC)
}

pub fn encrypt_cloud_payload(compressed: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.len() != CLOUD_SHARE_KEY_LEN {
        return Err(color_eyre::eyre::eyre!(
            "Invalid share key length: expected {} bytes, got {}",
            CLOUD_SHARE_KEY_LEN,
            key.len()
        ));
    }

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| color_eyre::eyre::eyre!("Failed to initialize cloud share cipher: {e}"))?;

    let mut nonce = [0_u8; CLOUD_SHARE_NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);

    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), compressed)
        .map_err(|_| color_eyre::eyre::eyre!("Failed to encrypt cloud share payload"))?;

    let mut output = Vec::with_capacity(CLOUD_SHARE_MAGIC.len() + nonce.len() + ciphertext.len());
    output.extend_from_slice(CLOUD_SHARE_MAGIC);
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

pub fn decrypt_cloud_payload(payload: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.len() != CLOUD_SHARE_KEY_LEN {
        return Err(color_eyre::eyre::eyre!(
            "Invalid share key length: expected {} bytes, got {}",
            CLOUD_SHARE_KEY_LEN,
            key.len()
        ));
    }

    if !is_encrypted_cloud_payload(payload) {
        return Err(color_eyre::eyre::eyre!(
            "Payload is not an encrypted cloud share"
        ));
    }

    let min_len = CLOUD_SHARE_MAGIC.len() + CLOUD_SHARE_NONCE_LEN + 16;
    if payload.len() < min_len {
        return Err(color_eyre::eyre::eyre!(
            "Encrypted payload is too short or corrupted"
        ));
    }

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| color_eyre::eyre::eyre!("Failed to initialize cloud share cipher: {e}"))?;
    let nonce_start = CLOUD_SHARE_MAGIC.len();
    let nonce_end = nonce_start + CLOUD_SHARE_NONCE_LEN;
    let nonce = Nonce::from_slice(&payload[nonce_start..nonce_end]);
    let ciphertext = &payload[nonce_end..];

    cipher.decrypt(nonce, ciphertext).map_err(|_| {
        color_eyre::eyre::eyre!(
            "Failed to decrypt cloud share payload. Ensure the link key is correct."
        )
    })
}

pub fn decode_cloud_payload(payload: &[u8], key: Option<&[u8]>) -> Result<Vec<u8>> {
    if is_encrypted_cloud_payload(payload) {
        let key = key.ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "This shared session is encrypted. Add '#{}=<key>' to the link.",
                SHARE_KEY_PARAM
            )
        })?;
        decrypt_cloud_payload(payload, key)
    } else {
        Ok(payload.to_vec())
    }
}

#[must_use]
pub fn attach_key_to_share_url(url: &str, key: &[u8]) -> String {
    let encoded_key = encode_cloud_share_key(key);
    match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_fragment(Some(&format!("{SHARE_KEY_PARAM}={encoded_key}")));
            parsed.to_string()
        }
        Err(_) => format!("{url}#{SHARE_KEY_PARAM}={encoded_key}"),
    }
}

pub fn parse_cloud_share_locator(input: &str) -> Result<CloudShareLocator> {
    let parsed = reqwest::Url::parse(input.trim())
        .map_err(|e| color_eyre::eyre::eyre!("Invalid URL: {e}"))?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(color_eyre::eyre::eyre!(
            "Unsupported URL scheme '{}'. Expected http or https.",
            parsed.scheme()
        ));
    }

    let path = parsed.path().trim_end_matches('/');
    let id = path
        .strip_prefix("/s/")
        .or_else(|| path.strip_prefix("/api/sessions/"))
        .ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "Unsupported share URL path '{}'. Expected '/s/<id>' or '/api/sessions/<id>'.",
                parsed.path()
            )
        })?;

    if !is_valid_cloud_share_id(id) {
        return Err(color_eyre::eyre::eyre!("Invalid cloud share ID: '{id}'"));
    }

    let mut key_encoded = parsed.query_pairs().find_map(|(k, v)| {
        if k == SHARE_KEY_PARAM || k == "key" {
            Some(v.into_owned())
        } else {
            None
        }
    });

    if key_encoded.is_none() {
        key_encoded = parsed.fragment().and_then(parse_key_from_fragment);
    }

    let key = key_encoded
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(decode_cloud_share_key)
        .transpose()?;

    let api_url = parsed
        .join(&format!("/api/sessions/{id}"))
        .map_err(|e| color_eyre::eyre::eyre!("Failed to build API URL from share link: {e}"))?
        .to_string();

    Ok(CloudShareLocator {
        id: id.to_string(),
        api_url,
        key,
    })
}

pub fn fetch_shared_session_from_cloud_link(link: &str) -> Result<SharedSession> {
    let locator = parse_cloud_share_locator(link)?;
    let payload = download_cloud_payload(&locator.api_url).map_err(|e| {
        color_eyre::eyre::eyre!("Failed to download shared session {}: {e}", locator.id)
    })?;
    let compressed = decode_cloud_payload(&payload, locator.key.as_deref())?;
    decompress_session(&compressed)
}

fn is_valid_cloud_share_id(id: &str) -> bool {
    id.len() == 12
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn parse_key_from_fragment(fragment: &str) -> Option<String> {
    if fragment.is_empty() {
        return None;
    }

    if !fragment.contains('=') {
        return Some(fragment.to_string());
    }

    for pair in fragment.split('&') {
        let mut parts = pair.splitn(2, '=');
        let Some(name) = parts.next() else {
            continue;
        };
        if name == SHARE_KEY_PARAM || name == "key" {
            if let Some(value) = parts.next() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn download_cloud_payload(api_url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::new();
    let response = client.get(api_url).send()?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(color_eyre::eyre::eyre!(
            "Download failed: {} - {}",
            status,
            body
        ));
    }

    Ok(response.bytes()?.to_vec())
}

/// Compress a session using zstd level 3.
/// Returns the compressed bytes.
pub fn compress_session(session: &Session) -> Result<Vec<u8>> {
    let shared = SharedSession::new(session.clone());
    let json = serde_json::to_vec(&shared)?;

    let mut encoder = zstd::Encoder::new(Vec::new(), 3)?;
    encoder.write_all(&json)?;
    let compressed = encoder.finish()?;

    Ok(compressed)
}

/// Decompress a session from zstd-compressed bytes.
#[allow(dead_code)]
pub fn decompress_session(data: &[u8]) -> Result<SharedSession> {
    let mut decoder = zstd::Decoder::new(data)?;
    let mut json = Vec::new();
    decoder.read_to_end(&mut json)?;
    let shared: SharedSession = serde_json::from_slice(&json)?;
    Ok(shared)
}

/// Build a portable single-file share payload.
pub fn build_share_file(
    session: &Session,
    mode: ShareExportMode,
    resume_input: Option<&ResumeBundleInput>,
) -> Result<Vec<u8>> {
    let filtered = filter_session(session, mode);
    let resume_bundle = if mode.is_resumable() {
        resume_input.map(build_resume_bundle).transpose()?
    } else {
        None
    };

    let payload = PortableSharedFile {
        version: 1,
        shared_at: Utc::now().to_rfc3339(),
        mode,
        session: filtered,
        resume_bundle,
    };

    let json = serde_json::to_vec(&payload)?;
    let mut encoder = zstd::Encoder::new(Vec::new(), 3)?;
    encoder.write_all(&json)?;
    Ok(encoder.finish()?)
}

/// Read a portable single-file share payload.
#[allow(dead_code)]
pub fn read_share_file(data: &[u8]) -> Result<PortableSharedFile> {
    let mut decoder = zstd::Decoder::new(data)?;
    let mut json = Vec::new();
    decoder.read_to_end(&mut json)?;
    Ok(serde_json::from_slice(&json)?)
}

/// Read and decode a portable share payload from disk.
pub fn read_share_file_from_path(path: &Path) -> Result<PortableSharedFile> {
    let data = fs::read(path)?;
    read_share_file(&data)
}

fn filter_session(session: &Session, mode: ShareExportMode) -> Session {
    let turns = session
        .turns
        .iter()
        .map(|turn| match mode {
            ShareExportMode::PromptResponseOnly => Turn {
                id: turn.id.clone(),
                timestamp: turn.timestamp.clone(),
                user_prompt: turn.user_prompt.clone(),
                thinking: None,
                tool_invocations: Vec::new(),
                response: turn.response.clone(),
                model: turn.model.clone(),
            },
            ShareExportMode::PromptResponseAndDiff => Turn {
                id: turn.id.clone(),
                timestamp: turn.timestamp.clone(),
                user_prompt: turn.user_prompt.clone(),
                thinking: None,
                tool_invocations: diff_only_invocations(turn),
                response: turn.response.clone(),
                model: turn.model.clone(),
            },
            ShareExportMode::FullSession => turn.clone(),
        })
        .collect();

    Session {
        id: session.id.clone(),
        name: session.name.clone(),
        source: session.source.clone(),
        project_path: session.project_path.clone(),
        turns,
    }
}

fn diff_only_invocations(turn: &Turn) -> Vec<ToolInvocation> {
    let mut output = Vec::new();

    for tool in &turn.tool_invocations {
        if let Some(diff) = tool.tool_type.diff() {
            output.push(ToolInvocation {
                id: tool.id.clone(),
                tool_type: ToolType::FileEdit {
                    path: tool_path(tool).unwrap_or_else(|| "unknown".to_string()),
                    old_content: None,
                    new_content: None,
                    diff: Some(diff),
                },
                input_display: "[redacted]".to_string(),
                output_display: "[diff-only export]".to_string(),
                raw_input: Value::Null,
                raw_output: None,
            });
        }

        if let ToolType::Task {
            subagent_turns,
            subagent_type,
            ..
        } = &tool.tool_type
        {
            let prefix = subagent_type.as_deref().unwrap_or("subagent");
            for (sub_idx, subturn) in subagent_turns.iter().enumerate() {
                for subtool in &subturn.tool_invocations {
                    if let Some(diff) = subtool.tool_type.diff() {
                        let path = tool_path(subtool)
                            .map(|p| format!("[{prefix}] {p}"))
                            .unwrap_or_else(|| format!("[{prefix}] unknown"));
                        output.push(ToolInvocation {
                            id: format!("{}-sub-{}-{}", tool.id, sub_idx, subtool.id),
                            tool_type: ToolType::FileEdit {
                                path,
                                old_content: None,
                                new_content: None,
                                diff: Some(diff),
                            },
                            input_display: "[redacted]".to_string(),
                            output_display: "[diff-only export]".to_string(),
                            raw_input: Value::Null,
                            raw_output: None,
                        });
                    }
                }
            }
        }
    }

    output
}

fn tool_path(tool: &ToolInvocation) -> Option<String> {
    match &tool.tool_type {
        ToolType::FileRead { path, .. }
        | ToolType::FileWrite { path, .. }
        | ToolType::FileEdit { path, .. } => Some(path.clone()),
        _ => None,
    }
}

fn build_resume_bundle(input: &ResumeBundleInput) -> Result<ResumeBundle> {
    Ok(ResumeBundle {
        source: input.source,
        resume_session_id: input.resume_session_id.clone(),
        resume_command: input.resume_command.clone(),
        project_path_hint: input.project_path_hint.display().to_string(),
        artifacts: collect_resume_artifacts(input)?,
    })
}

fn collect_resume_artifacts(input: &ResumeBundleInput) -> Result<Vec<ResumeArtifact>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for path in &input.session_paths {
        candidates.push(path.clone());
        if input.source == ResumeSource::Claude {
            collect_claude_sidecars(path, &mut candidates);
        }
    }

    let mut seen = HashSet::new();
    let mut deduped: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|p| p.is_file())
        .filter(|p| seen.insert(p.clone()))
        .collect();
    deduped.sort();

    let artifacts = deduped
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path)?;
            Ok(ResumeArtifact {
                restore_path: restore_path_from_home(&path),
                source_path: path.display().to_string(),
                content_base64: BASE64.encode(&bytes),
                size_bytes: bytes.len(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(artifacts)
}

fn collect_claude_sidecars(session_path: &Path, out: &mut Vec<PathBuf>) {
    let Some(parent) = session_path.parent() else {
        return;
    };
    let Some(stem) = session_path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let sidecar_root = parent.join(stem);
    if !sidecar_root.is_dir() {
        return;
    }
    collect_files_recursive(&sidecar_root, out);
}

fn collect_files_recursive(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn restore_path_from_home(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            return rel.display().to_string();
        }
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session.jsonl")
        .to_string()
}

#[must_use]
pub fn default_share_file_path(session_name: &str, mode: ShareExportMode) -> PathBuf {
    let base_dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
    let mode_tag = match mode {
        ShareExportMode::PromptResponseOnly => "pr",
        ShareExportMode::PromptResponseAndDiff => "prdiff",
        ShareExportMode::FullSession => "full",
    };
    let safe_name = sanitize_filename(session_name);
    base_dir.join(format!(
        "vibereview-{safe_name}-{mode_tag}-{timestamp}.json.zst"
    ))
}

pub fn write_share_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, data)?;
    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Upload a cloud share payload to the configured share service.
/// Returns the upload response with ID and URL.
pub fn upload_session(payload: &[u8], api_url: &str) -> Result<UploadResponse> {
    let client = reqwest::blocking::Client::new();

    let response = client
        .post(api_url)
        .header("Content-Type", "application/octet-stream")
        .body(payload.to_vec())
        .send()?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(color_eyre::eyre::eyre!(
            "Upload failed: {} - {}",
            status,
            body
        ));
    }

    let upload_response: UploadResponse = response.json()?;
    Ok(upload_response)
}

/// Copy text to the system clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut clipboard = Clipboard::new()?;
    clipboard.set_text(text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SessionSource, Turn};

    #[test]
    fn test_compress_decompress_roundtrip() {
        let session = Session {
            id: "test-123".to_string(),
            name: "Test Session".to_string(),
            source: SessionSource::ClaudeCode {
                version: Some("1.0".to_string()),
            },
            project_path: None,
            turns: vec![Turn {
                id: "turn-1".to_string(),
                timestamp: Some("2024-01-01T00:00:00Z".to_string()),
                user_prompt: "Hello, world!".to_string(),
                thinking: Some("Thinking...".to_string()),
                tool_invocations: vec![],
                response: "Hi there!".to_string(),
                model: Some("claude-3".to_string()),
            }],
        };

        let compressed = compress_session(&session).unwrap();
        let decompressed = decompress_session(&compressed).unwrap();

        assert_eq!(decompressed.version, 1);
        assert_eq!(decompressed.session.id, "test-123");
        assert_eq!(decompressed.session.turns.len(), 1);
        assert_eq!(decompressed.session.turns[0].user_prompt, "Hello, world!");
    }

    #[test]
    fn test_share_mode_prompt_response_only_strips_tools() {
        let session = Session {
            id: "s1".to_string(),
            name: "test".to_string(),
            source: SessionSource::Other {
                name: "x".to_string(),
            },
            project_path: None,
            turns: vec![Turn {
                id: "t1".to_string(),
                timestamp: None,
                user_prompt: "u".to_string(),
                thinking: Some("think".to_string()),
                tool_invocations: vec![ToolInvocation {
                    id: "tool1".to_string(),
                    tool_type: ToolType::Command {
                        command: "echo hi".to_string(),
                        stdout: Some("hi".to_string()),
                        stderr: None,
                        exit_code: Some(0),
                    },
                    input_display: "in".to_string(),
                    output_display: "out".to_string(),
                    raw_input: Value::Null,
                    raw_output: None,
                }],
                response: "r".to_string(),
                model: None,
            }],
        };

        let data = build_share_file(&session, ShareExportMode::PromptResponseOnly, None).unwrap();
        let parsed = read_share_file(&data).unwrap();
        assert_eq!(parsed.mode, ShareExportMode::PromptResponseOnly);
        assert_eq!(parsed.session.turns.len(), 1);
        assert!(parsed.session.turns[0].tool_invocations.is_empty());
        assert!(parsed.session.turns[0].thinking.is_none());
    }

    #[test]
    fn test_share_mode_prompt_response_diff_keeps_only_diff_tools() {
        let session = Session {
            id: "s1".to_string(),
            name: "test".to_string(),
            source: SessionSource::Other {
                name: "x".to_string(),
            },
            project_path: None,
            turns: vec![Turn {
                id: "t1".to_string(),
                timestamp: None,
                user_prompt: "u".to_string(),
                thinking: Some("think".to_string()),
                tool_invocations: vec![ToolInvocation {
                    id: "tool1".to_string(),
                    tool_type: ToolType::FileEdit {
                        path: "src/main.rs".to_string(),
                        old_content: Some("a".to_string()),
                        new_content: Some("b".to_string()),
                        diff: Some("--- old\n+++ new\n-a\n+b".to_string()),
                    },
                    input_display: "in".to_string(),
                    output_display: "out".to_string(),
                    raw_input: Value::Null,
                    raw_output: None,
                }],
                response: "r".to_string(),
                model: None,
            }],
        };

        let data =
            build_share_file(&session, ShareExportMode::PromptResponseAndDiff, None).unwrap();
        let parsed = read_share_file(&data).unwrap();
        assert_eq!(parsed.mode, ShareExportMode::PromptResponseAndDiff);
        assert_eq!(parsed.session.turns.len(), 1);
        assert_eq!(parsed.session.turns[0].tool_invocations.len(), 1);
        let only = &parsed.session.turns[0].tool_invocations[0];
        assert!(matches!(only.tool_type, ToolType::FileEdit { .. }));
        assert_eq!(only.input_display, "[redacted]");
    }

    #[test]
    fn test_cloud_share_key_roundtrip() {
        let key = [7_u8; 32];
        let encoded = encode_cloud_share_key(&key);
        let decoded = decode_cloud_share_key(&encoded).unwrap();
        assert_eq!(decoded, key.to_vec());
    }

    #[test]
    fn test_encrypt_decrypt_cloud_payload_roundtrip() {
        let key = [9_u8; 32];
        let payload = b"hello encrypted world";
        let encrypted = encrypt_cloud_payload(payload, &key).unwrap();
        assert!(is_encrypted_cloud_payload(&encrypted));

        let decrypted = decrypt_cloud_payload(&encrypted, &key).unwrap();
        assert_eq!(decrypted, payload);
    }

    #[test]
    fn test_decode_encrypted_payload_without_key_fails() {
        let key = [5_u8; 32];
        let payload = b"hello";
        let encrypted = encrypt_cloud_payload(payload, &key).unwrap();
        assert!(decode_cloud_payload(&encrypted, None).is_err());
    }

    #[test]
    fn test_parse_cloud_share_locator_extracts_key() {
        let key = [3_u8; 32];
        let encoded = encode_cloud_share_key(&key);
        let url = format!("https://share.example/s/abc123DEF_45#k={encoded}");

        let locator = parse_cloud_share_locator(&url).unwrap();
        assert_eq!(locator.id, "abc123DEF_45");
        assert_eq!(
            locator.api_url,
            "https://share.example/api/sessions/abc123DEF_45"
        );
        assert_eq!(locator.key.unwrap(), key.to_vec());
    }
}
