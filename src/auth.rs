//! GitHub authentication helpers for cloud features.

use chrono::Utc;
use color_eyre::{eyre::eyre, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

pub const GITHUB_TOKEN_ENV: &str = "VIBEREVIEW_GITHUB_TOKEN";
pub const GITHUB_CLIENT_ID_ENV: &str = "VIBEREVIEW_GITHUB_CLIENT_ID";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthState {
    pub github_access_token: String,
    pub github_login: String,
    pub github_user_id: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    pub id: u64,
}

#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
}

fn auth_state_path() -> PathBuf {
    if let Some(mut dir) = dirs::config_dir() {
        dir.push("vibereview");
        dir.push("auth.json");
        return dir;
    }
    if let Some(mut home) = dirs::home_dir() {
        home.push(".vibereview-auth.json");
        return home;
    }
    PathBuf::from(".vibereview-auth.json")
}

pub fn save_auth_state(state: &AuthState) -> Result<()> {
    let path = auth_state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(state)?;
    fs::write(path, data)?;
    Ok(())
}

pub fn load_auth_state() -> Result<Option<AuthState>> {
    let path = auth_state_path();
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    let state = serde_json::from_slice::<AuthState>(&bytes)?;
    Ok(Some(state))
}

#[must_use]
pub fn load_auth_token() -> Option<String> {
    if let Ok(token) = std::env::var(GITHUB_TOKEN_ENV) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }
    load_auth_state()
        .ok()
        .flatten()
        .map(|s| s.github_access_token)
}

pub fn start_device_flow(client_id: &str) -> Result<DeviceCodeResponse> {
    let client = Client::new();
    let response = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("User-Agent", "vibereview")
        .form(&[("client_id", client_id), ("scope", "read:user")])
        .send()?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(eyre!(
            "Failed to start GitHub device flow: {} - {}",
            status,
            body
        ));
    }

    Ok(response.json()?)
}

pub fn poll_device_flow_access_token(
    client_id: &str,
    device: &DeviceCodeResponse,
) -> Result<String> {
    let client = Client::new();
    let started = Instant::now();
    let mut interval = device.interval.max(1);

    while started.elapsed() < Duration::from_secs(device.expires_in) {
        thread::sleep(Duration::from_secs(interval));

        let response = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("User-Agent", "vibereview")
            .form(&[
                ("client_id", client_id),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(eyre!("GitHub token polling failed: {} - {}", status, body));
        }

        let token_response: TokenResponse = response.json()?;
        if let Some(token) = token_response.access_token {
            return Ok(token);
        }

        match token_response.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = interval.saturating_add(5);
            }
            Some("access_denied") => {
                return Err(eyre!("GitHub login was cancelled."));
            }
            Some("expired_token") | Some("token_expired") => {
                return Err(eyre!("GitHub device code expired. Please run login again."));
            }
            Some(other) => {
                return Err(eyre!("GitHub login failed: {}", other));
            }
            None => {
                return Err(eyre!(
                    "GitHub login failed: token response did not include access token"
                ));
            }
        }
    }

    Err(eyre!("GitHub login timed out. Please run login again."))
}

pub fn fetch_github_user(access_token: &str) -> Result<GitHubUser> {
    let client = Client::new();
    let response = client
        .get("https://api.github.com/user")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "vibereview")
        .bearer_auth(access_token)
        .send()?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(eyre!(
            "Failed to fetch GitHub profile: {} - {}",
            status,
            body
        ));
    }

    Ok(response.json()?)
}

pub fn login_with_github(client_id: &str) -> Result<AuthState> {
    let device = start_device_flow(client_id)?;

    println!("Open: {}", device.verification_uri);
    println!("Code: {}", device.user_code);
    println!("Waiting for authorization...");

    let access_token = poll_device_flow_access_token(client_id, &device)?;
    let user = fetch_github_user(&access_token)?;

    Ok(AuthState {
        github_access_token: access_token,
        github_login: user.login,
        github_user_id: user.id,
        created_at: Utc::now().to_rfc3339(),
    })
}
