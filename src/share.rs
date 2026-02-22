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
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const SHARE_KEY_PARAM: &str = "k";

pub const CLOUD_SHARE_API_URL: &str = "https://vibereview.trustme.workers.dev/api/sessions";
const GITHUB_CLIENT_ID_PATH: &str = "/api/auth/github/client-id";
const LIST_UPLOADS_PATH: &str = "/api/sessions";
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
    #[serde(default)]
    pub reused: bool,
}

#[derive(Debug, Clone)]
pub struct CloudShareLocator {
    pub id: String,
    pub api_url: String,
    pub key: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadListResponse {
    pub uploads: Vec<UploadListItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadListItem {
    pub id: String,
    pub url: String,
    pub fingerprint: String,
    pub security: String,
    pub session_name: Option<String>,
    pub turn_count: Option<usize>,
    pub uploaded_at: String,
}

/// Returns configured cloud share API URL if available.
#[must_use]
pub fn cloud_share_api_url() -> Option<String> {
    Some(CLOUD_SHARE_API_URL.to_string())
}

fn cloud_endpoint_url(absolute_path: &str) -> Result<String> {
    let base = reqwest::Url::parse(CLOUD_SHARE_API_URL).map_err(|e| {
        color_eyre::eyre::eyre!(
            "Invalid hardcoded cloud API URL '{}': {e}",
            CLOUD_SHARE_API_URL
        )
    })?;
    let endpoint = base.join(absolute_path).map_err(|e| {
        color_eyre::eyre::eyre!(
            "Failed to build endpoint '{}' from hardcoded cloud API URL '{}': {e}",
            absolute_path,
            CLOUD_SHARE_API_URL
        )
    })?;
    Ok(endpoint.to_string())
}

pub fn fetch_github_client_id() -> Result<String> {
    let endpoint = cloud_endpoint_url(GITHUB_CLIENT_ID_PATH)?;
    let client = reqwest::blocking::Client::new();
    let response = client.get(endpoint).send()?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(color_eyre::eyre::eyre!(
            "Failed to fetch GitHub client ID: {} - {}",
            status,
            body
        ));
    }

    #[derive(Deserialize)]
    struct ClientIdResponse {
        client_id: String,
    }

    let parsed: ClientIdResponse = response.json()?;
    let client_id = parsed.client_id.trim().to_string();
    if client_id.is_empty() {
        return Err(color_eyre::eyre::eyre!(
            "Cloud auth endpoint returned an empty GitHub client ID"
        ));
    }
    Ok(client_id)
}

pub fn list_uploads(auth_token: &str) -> Result<UploadListResponse> {
    let endpoint = cloud_endpoint_url(LIST_UPLOADS_PATH)?;
    let client = reqwest::blocking::Client::new();
    let response = client.get(endpoint).bearer_auth(auth_token).send()?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(color_eyre::eyre::eyre!(
            "Failed to list uploads: {} - {}",
            status,
            body
        ));
    }

    Ok(response.json()?)
}

pub fn session_fingerprint(session: &Session) -> Result<String> {
    let data = serde_json::to_vec(&session.turns)?;
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    Ok(output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretScanFinding {
    pub detector: &'static str,
    pub location: String,
    pub sample: String,
}

impl SecretScanFinding {
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{} in {} ({})", self.detector, self.location, self.sample)
    }
}

/// Scan a parsed session for high-confidence secrets before cloud upload.
///
/// The scanner is intentionally conservative and returns only likely credential
/// matches (token formats, private-key blocks, and obvious bearer values).
#[must_use]
pub fn scan_session_for_secrets(session: &Session, max_findings: usize) -> Vec<SecretScanFinding> {
    let max_findings = max_findings.max(1);
    let mut findings = Vec::new();
    let mut seen = HashSet::new();

    scan_text_for_secrets(
        "session name",
        &session.name,
        &mut findings,
        &mut seen,
        max_findings,
    );

    for (turn_idx, turn) in session.turns.iter().enumerate() {
        let turn_number = turn_idx + 1;
        scan_text_for_secrets(
            &format!("turn {turn_number} prompt"),
            &turn.user_prompt,
            &mut findings,
            &mut seen,
            max_findings,
        );
        if findings.len() >= max_findings {
            return findings;
        }

        if let Some(thinking) = &turn.thinking {
            scan_text_for_secrets(
                &format!("turn {turn_number} thinking"),
                thinking,
                &mut findings,
                &mut seen,
                max_findings,
            );
            if findings.len() >= max_findings {
                return findings;
            }
        }

        scan_text_for_secrets(
            &format!("turn {turn_number} response"),
            &turn.response,
            &mut findings,
            &mut seen,
            max_findings,
        );
        if findings.len() >= max_findings {
            return findings;
        }

        for (tool_idx, tool) in turn.tool_invocations.iter().enumerate() {
            let tool_number = tool_idx + 1;
            let prefix = format!("turn {turn_number} tool {tool_number}");

            scan_text_for_secrets(
                &format!("{prefix} input"),
                &tool.input_display,
                &mut findings,
                &mut seen,
                max_findings,
            );
            if findings.len() >= max_findings {
                return findings;
            }

            scan_text_for_secrets(
                &format!("{prefix} output"),
                &tool.output_display,
                &mut findings,
                &mut seen,
                max_findings,
            );
            if findings.len() >= max_findings {
                return findings;
            }

            if let Ok(tool_json) = serde_json::to_string(&tool.tool_type) {
                scan_text_for_secrets(
                    &format!("{prefix} metadata"),
                    &tool_json,
                    &mut findings,
                    &mut seen,
                    max_findings,
                );
                if findings.len() >= max_findings {
                    return findings;
                }
            }

            if !tool.raw_input.is_null() {
                scan_text_for_secrets(
                    &format!("{prefix} raw input"),
                    &tool.raw_input.to_string(),
                    &mut findings,
                    &mut seen,
                    max_findings,
                );
                if findings.len() >= max_findings {
                    return findings;
                }
            }

            if let Some(raw_output) = &tool.raw_output {
                scan_text_for_secrets(
                    &format!("{prefix} raw output"),
                    &raw_output.to_string(),
                    &mut findings,
                    &mut seen,
                    max_findings,
                );
                if findings.len() >= max_findings {
                    return findings;
                }
            }
        }
    }

    findings
}

/// Scan raw source session files for likely secrets.
///
/// This is used as a fallback when parsed-session scanning misses values that
/// may have been truncated or transformed by parsers.
#[must_use]
pub fn scan_paths_for_secrets(paths: &[PathBuf], max_findings: usize) -> Vec<SecretScanFinding> {
    let max_findings = max_findings.max(1);
    let mut findings = Vec::new();
    let mut seen = HashSet::new();

    for path in paths {
        if findings.len() >= max_findings {
            break;
        }

        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let location = format!("source file {}", path.display());
        scan_text_for_secrets(&location, &text, &mut findings, &mut seen, max_findings);
    }

    findings
}

fn scan_text_for_secrets(
    location: &str,
    text: &str,
    findings: &mut Vec<SecretScanFinding>,
    seen: &mut HashSet<String>,
    max_findings: usize,
) {
    if text.is_empty() || findings.len() >= max_findings {
        return;
    }

    if contains_private_key_block(text) {
        record_secret_finding(
            findings,
            seen,
            max_findings,
            "Private key block",
            location,
            "-----BEGIN ... PRIVATE KEY-----",
        );
        if findings.len() >= max_findings {
            return;
        }
    }

    for token in text.split(|ch: char| !is_secret_token_char(ch)) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }

        if let Some((detector, secret)) = detect_secret_token(token) {
            record_secret_finding(findings, seen, max_findings, detector, location, secret);
            if findings.len() >= max_findings {
                return;
            }
        }
    }

    for line in text.lines() {
        if let Some(token) = extract_bearer_token(line) {
            record_secret_finding(
                findings,
                seen,
                max_findings,
                "Bearer token",
                location,
                token,
            );
            if findings.len() >= max_findings {
                return;
            }
        }

        if let Some(value) = extract_labeled_secret(line) {
            record_secret_finding(
                findings,
                seen,
                max_findings,
                "Labeled secret value",
                location,
                value,
            );
            if findings.len() >= max_findings {
                return;
            }
        }
    }
}

fn record_secret_finding(
    findings: &mut Vec<SecretScanFinding>,
    seen: &mut HashSet<String>,
    max_findings: usize,
    detector: &'static str,
    location: &str,
    secret: &str,
) {
    if findings.len() >= max_findings {
        return;
    }
    if looks_like_placeholder(secret) {
        return;
    }

    let dedupe_key = format!("{detector}:{secret}");
    if !seen.insert(dedupe_key) {
        return;
    }

    findings.push(SecretScanFinding {
        detector,
        location: location.to_string(),
        sample: redact_secret(secret),
    });
}

fn contains_private_key_block(text: &str) -> bool {
    text.contains("-----BEGIN ") && text.contains(" PRIVATE KEY-----")
}

fn is_secret_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '+' | '/')
}

fn is_secret_value_char(ch: char) -> bool {
    is_secret_token_char(ch) || ch == '='
}

fn detect_secret_token(token: &str) -> Option<(&'static str, &str)> {
    if looks_like_placeholder(token) {
        return None;
    }

    const GH_PREFIXES: [&str; 5] = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
    for prefix in GH_PREFIXES {
        if let Some(rest) = token.strip_prefix(prefix) {
            if rest.len() == 36 && rest.chars().all(|ch| ch.is_ascii_alphanumeric()) {
                return Some(("GitHub token", token));
            }
        }
    }

    if let Some(rest) = token.strip_prefix("github_pat_") {
        if rest.len() >= 20
            && rest
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Some(("GitHub fine-grained token", token));
        }
    }

    if let Some(rest) = token.strip_prefix("sk-ant-") {
        if rest.len() >= 20
            && rest
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Some(("Anthropic API key", token));
        }
    }

    if let Some(rest) = token.strip_prefix("sk-proj-") {
        if rest.len() >= 20
            && rest
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Some(("OpenAI API key", token));
        }
    }

    if token.len() == 20
        && (token.starts_with("AKIA") || token.starts_with("ASIA"))
        && token
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
    {
        return Some(("AWS access key ID", token));
    }

    if token.starts_with("AIza")
        && token.len() == 39
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Some(("Google API key", token));
    }

    const SLACK_PREFIXES: [&str; 5] = ["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"];
    for prefix in SLACK_PREFIXES {
        if let Some(rest) = token.strip_prefix(prefix) {
            if rest.len() >= 24
                && rest
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
            {
                return Some(("Slack token", token));
            }
        }
    }

    if let Some(rest) = token.strip_prefix("sk-") {
        if rest.len() >= 24
            && rest
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            && rest.chars().any(|ch| ch.is_ascii_digit())
        {
            return Some(("API key", token));
        }
    }

    None
}

fn extract_bearer_token(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    let idx = lower.find("bearer ")?;
    let token_part = line[idx + "bearer ".len()..].trim_start();
    let token = token_part
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | ']' | '}'))
        .next()
        .unwrap_or("")
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`');

    if looks_like_secret_value(token) {
        Some(token)
    } else {
        None
    }
}

fn extract_labeled_secret(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    if !contains_secret_label(&lower) {
        return None;
    }

    let value = line
        .split_once('=')
        .map(|(_, rhs)| rhs)
        .or_else(|| line.split_once(':').map(|(_, rhs)| rhs))?
        .trim_start();

    let token = value
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`')
        .trim_start_matches("Bearer ")
        .trim_start_matches("bearer ")
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | ']' | '}'))
        .next()
        .unwrap_or("");

    if looks_like_secret_value(token) {
        Some(token)
    } else {
        None
    }
}

fn contains_secret_label(lower_line: &str) -> bool {
    const LABELS: [&str; 9] = [
        "api_key",
        "apikey",
        "access_token",
        "auth_token",
        "client_secret",
        "secret_key",
        "authorization",
        "password",
        "token",
    ];
    LABELS.iter().any(|label| lower_line.contains(label))
}

fn looks_like_secret_value(value: &str) -> bool {
    if value.len() < 24 || looks_like_placeholder(value) || value.contains("://") {
        return false;
    }
    if !value.chars().all(is_secret_value_char) {
        return false;
    }
    let has_alpha = value.chars().any(|ch| ch.is_ascii_alphabetic());
    let has_digit = value.chars().any(|ch| ch.is_ascii_digit());
    has_alpha && has_digit
}

fn looks_like_placeholder(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.is_empty()
        || lower.contains("${")
        || lower.contains("{{")
        || lower.contains("<")
        || lower.contains(">")
        || lower.contains("example")
        || lower.contains("placeholder")
        || lower.contains("your_")
        || lower.contains("token_here")
        || lower.contains("redacted")
        || lower == "null"
        || lower == "undefined"
}

fn redact_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 10 {
        return "[hidden]".to_string();
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    format!("{prefix}...{suffix}")
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

#[must_use]
pub fn normalize_share_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"');
    let cleaned = trimmed
        .replace("\\/", "/")
        .replace("\\u0026", "&")
        .replace("\\u0023", "#");

    match reqwest::Url::parse(&cleaned) {
        Ok(url) => url.to_string(),
        Err(_) => cleaned,
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
                thinking_effort: turn.thinking_effort.clone(),
                tool_invocations: Vec::new(),
                response: turn.response.clone(),
                model: turn.model.clone(),
            },
            ShareExportMode::PromptResponseAndDiff => Turn {
                id: turn.id.clone(),
                timestamp: turn.timestamp.clone(),
                user_prompt: turn.user_prompt.clone(),
                thinking: None,
                thinking_effort: turn.thinking_effort.clone(),
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
pub fn upload_session(
    payload: &[u8],
    api_url: &str,
    auth_token: &str,
    fingerprint: &str,
    session_name: &str,
    turn_count: usize,
    security: &str,
) -> Result<UploadResponse> {
    let client = reqwest::blocking::Client::new();
    let safe_session_name: String = session_name
        .chars()
        .map(|ch| {
            if ch.is_ascii() && ch != '\n' && ch != '\r' {
                ch
            } else {
                '_'
            }
        })
        .collect();

    let response = client
        .post(api_url)
        .header("Content-Type", "application/octet-stream")
        .bearer_auth(auth_token)
        .header("X-Session-Fingerprint", fingerprint)
        .header("X-Session-Name", safe_session_name)
        .header("X-Session-Turn-Count", turn_count.to_string())
        .header("X-Share-Security", security)
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

    fn test_session_with_text(prompt: &str, response: &str) -> Session {
        Session {
            id: "scan-test".to_string(),
            name: "scan".to_string(),
            source: SessionSource::Other {
                name: "x".to_string(),
            },
            project_path: None,
            turns: vec![Turn {
                id: "t1".to_string(),
                timestamp: None,
                user_prompt: prompt.to_string(),
                thinking: None,
                thinking_effort: None,
                tool_invocations: Vec::new(),
                response: response.to_string(),
                model: None,
            }],
        }
    }

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
                thinking_effort: Some("high".to_string()),
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
                thinking_effort: Some("high".to_string()),
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
        assert_eq!(
            parsed.session.turns[0].thinking_effort,
            Some("high".to_string())
        );
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
                thinking_effort: Some("xhigh".to_string()),
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
        assert_eq!(
            parsed.session.turns[0].thinking_effort,
            Some("xhigh".to_string())
        );
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

    #[test]
    fn test_normalize_share_url_unescapes_json_style_slashes() {
        let raw = "https:\\/\\/share.example\\/s\\/abc123DEF_45";
        let normalized = normalize_share_url(raw);
        assert_eq!(normalized, "https://share.example/s/abc123DEF_45");
    }

    #[test]
    fn test_normalize_share_url_strips_wrapping_quotes() {
        let raw = "\"https://share.example/s/abc123DEF_45\"";
        let normalized = normalize_share_url(raw);
        assert_eq!(normalized, "https://share.example/s/abc123DEF_45");
    }

    #[test]
    fn test_session_fingerprint_is_deterministic() {
        let session = Session {
            id: "s1".to_string(),
            name: "Test".to_string(),
            source: SessionSource::Other {
                name: "x".to_string(),
            },
            project_path: None,
            turns: vec![Turn {
                id: "t1".to_string(),
                timestamp: None,
                user_prompt: "hello".to_string(),
                thinking: None,
                thinking_effort: None,
                tool_invocations: Vec::new(),
                response: "world".to_string(),
                model: None,
            }],
        };

        let first = session_fingerprint(&session).unwrap();
        let second = session_fingerprint(&session).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn test_session_fingerprint_ignores_session_metadata() {
        let turns = vec![Turn {
            id: "t1".to_string(),
            timestamp: None,
            user_prompt: "hello".to_string(),
            thinking: None,
            thinking_effort: None,
            tool_invocations: Vec::new(),
            response: "world".to_string(),
            model: None,
        }];

        let session_a = Session {
            id: "a".to_string(),
            name: "Session A".to_string(),
            source: SessionSource::Other {
                name: "x".to_string(),
            },
            project_path: None,
            turns: turns.clone(),
        };

        let session_b = Session {
            id: "b".to_string(),
            name: "Session B".to_string(),
            source: SessionSource::Other {
                name: "y".to_string(),
            },
            project_path: None,
            turns,
        };

        assert_eq!(
            session_fingerprint(&session_a).unwrap(),
            session_fingerprint(&session_b).unwrap()
        );
    }

    #[test]
    fn test_scan_session_for_secrets_detects_github_token() {
        let token = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let session = test_session_with_text(&format!("token={token}"), "ok");

        let findings = scan_session_for_secrets(&session, 5);
        assert!(!findings.is_empty());
        assert!(findings.iter().any(|f| f.detector == "GitHub token"));
    }

    #[test]
    fn test_scan_session_for_secrets_detects_private_key_block() {
        let response = "-----BEGIN PRIVATE KEY-----\nABCDEF\n-----END PRIVATE KEY-----";
        let session = test_session_with_text("hello", response);

        let findings = scan_session_for_secrets(&session, 5);
        assert!(findings.iter().any(|f| f.detector == "Private key block"));
    }

    #[test]
    fn test_scan_session_for_secrets_ignores_placeholders() {
        let prompt = "Authorization: Bearer ${API_KEY}\napi_key=YOUR_API_KEY_HERE";
        let session = test_session_with_text(prompt, "example only");

        let findings = scan_session_for_secrets(&session, 5);
        assert!(findings.is_empty());
    }

    #[test]
    fn test_scan_session_for_secrets_respects_limit() {
        let prompt = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\nAKIAABCDEFGHIJKLMNOP";
        let session = test_session_with_text(prompt, "ok");

        let findings = scan_session_for_secrets(&session, 1);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn test_scan_paths_for_secrets_detects_private_key_block() {
        use std::io::Write as _;

        let file_path = std::env::temp_dir().join(format!(
            "vibereview-secret-scan-{}-{}.jsonl",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));

        {
            let mut file = std::fs::File::create(&file_path).unwrap();
            writeln!(
                file,
                "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----"
            )
            .unwrap();
        }

        let findings = scan_paths_for_secrets(std::slice::from_ref(&file_path), 5);
        let _ = std::fs::remove_file(&file_path);

        assert!(
            findings.iter().any(|f| f.detector == "Private key block"),
            "expected private key block finding, got: {findings:?}"
        );
    }

    #[test]
    fn test_scan_paths_for_secrets_detects_github_token() {
        use std::io::Write as _;

        let file_path = std::env::temp_dir().join(format!(
            "vibereview-secret-scan-token-{}-{}.jsonl",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));

        {
            let mut file = std::fs::File::create(&file_path).unwrap();
            writeln!(file, "token=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789").unwrap();
        }

        let findings = scan_paths_for_secrets(std::slice::from_ref(&file_path), 5);
        let _ = std::fs::remove_file(&file_path);

        assert!(
            findings.iter().any(|f| f.detector == "GitHub token"),
            "expected GitHub token finding, got: {findings:?}"
        );
    }
}
