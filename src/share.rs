//! Session sharing functionality: compression, upload, and clipboard operations.

use crate::models::Session;
use arboard::Clipboard;
use chrono::Utc;
use color_eyre::Result;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// The base URL for the share service
/// TODO: Update with your Cloudflare Workers subdomain after first deploy
const SHARE_API_URL: &str = "https://vibereview.<your-subdomain>.workers.dev/api/sessions";

/// A shared session with metadata for the web viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedSession {
    /// Schema version for future compatibility
    pub version: u8,
    /// The session data
    pub session: Session,
    /// ISO 8601 timestamp of when the session was shared
    pub shared_at: String,
}

impl SharedSession {
    pub fn new(session: Session) -> Self {
        Self {
            version: 1,
            session,
            shared_at: Utc::now().to_rfc3339(),
        }
    }
}

/// Response from the upload API
#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    pub id: String,
    pub url: String,
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

/// Upload a compressed session to the share service.
/// Returns the upload response with ID and URL.
pub fn upload_session(compressed: &[u8]) -> Result<UploadResponse> {
    let client = reqwest::blocking::Client::new();

    let response = client
        .post(SHARE_API_URL)
        .header("Content-Type", "application/octet-stream")
        .header("Content-Encoding", "zstd")
        .body(compressed.to_vec())
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
            source: SessionSource::ClaudeCode { version: Some("1.0".to_string()) },
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
}
