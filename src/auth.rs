//! OAuth authentication for provider-backed subscriptions.

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};

const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const ANTHROPIC_SCOPES: &str = "org:create_api_key user:profile user:inference";

const ANTIGRAVITY_AUTH_BASE_URLS: &[&str] = &[
    "https://auth.agentgateway.dev",
    "https://auth.agentgateway.com",
];
const ANTIGRAVITY_OAUTH_CLIENT_ID: &str =
    "336323648001-c5fqumriim5d2udondps7ce2s4155vsi.apps.googleusercontent.com";
const ANTIGRAVITY_OAUTH_CLIENT_SECRET: &str = "R_PiQ6exBlywQqD_jj4g5v2B";
const ANTIGRAVITY_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const ANTIGRAVITY_MAX_POLL_ATTEMPTS: usize = 60;
const ANTIGRAVITY_POLL_INTERVAL_SECS: u64 = 5;

/// Stored Anthropic OAuth credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Expiry as Unix timestamp in milliseconds.
    pub expires_at: i64,
}

impl OAuthCredentials {
    /// Check if the access token is expired or about to expire (within 5 minutes).
    pub fn is_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        let buffer = 5 * 60 * 1000;
        now >= self.expires_at - buffer
    }

    /// Refresh the access token. Returns new credentials with updated tokens.
    pub async fn refresh(&self) -> Result<Self> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": self.refresh_token,
            "client_id": ANTHROPIC_CLIENT_ID,
        });

        let response = client
            .post(ANTHROPIC_TOKEN_URL)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send refresh request")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read refresh response")?;

        if !status.is_success() {
            anyhow::bail!("token refresh failed ({}): {}", status, text);
        }

        let json: AnthropicTokenResponse =
            serde_json::from_str(&text).context("failed to parse refresh response")?;

        Ok(Self {
            access_token: json.access_token,
            refresh_token: json.refresh_token,
            expires_at: chrono::Utc::now().timestamp_millis() + json.expires_in * 1000,
        })
    }
}

#[derive(Deserialize)]
struct AnthropicTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

/// Stored Antigravity OAuth credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntigravityCredentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Expiry as Unix timestamp in milliseconds.
    pub expires_at: i64,
    pub token_type: Option<String>,
    pub scope: Option<String>,
    pub project_id: String,
}

impl AntigravityCredentials {
    /// Check if the access token is expired or about to expire (within 5 minutes).
    pub fn is_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        let buffer = 5 * 60 * 1000;
        now >= self.expires_at - buffer
    }

    /// Refresh the access token using the stored refresh token.
    pub async fn refresh(&self) -> Result<Self> {
        let refresh_token = self
            .refresh_token
            .as_ref()
            .context("missing refresh_token in antigravity credentials")?;

        let client = reqwest::Client::new();
        let body = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", ANTIGRAVITY_OAUTH_CLIENT_ID),
            ("client_secret", ANTIGRAVITY_OAUTH_CLIENT_SECRET),
        ];

        let response = client
            .post(ANTIGRAVITY_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&body)
            .send()
            .await
            .context("failed to send antigravity refresh request")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read antigravity refresh response")?;

        if !status.is_success() {
            anyhow::bail!("antigravity token refresh failed ({}): {}", status, text);
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).context("failed to parse antigravity refresh response")?;

        let access_token = json["access_token"]
            .as_str()
            .context("antigravity refresh response missing access_token")?
            .to_string();
        let expires_in_secs = json["expires_in"].as_i64().unwrap_or(3600);
        let expires_at = chrono::Utc::now().timestamp_millis() + expires_in_secs * 1000;
        let refreshed_refresh_token = json["refresh_token"]
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| self.refresh_token.clone());
        let token_type = json["token_type"]
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| self.token_type.clone());
        let scope = json["scope"]
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| self.scope.clone());

        Ok(Self {
            access_token,
            refresh_token: refreshed_refresh_token,
            expires_at,
            token_type,
            scope,
            project_id: self.project_id.clone(),
        })
    }
}

#[derive(Deserialize)]
struct AntigravityAuthStartResponse {
    url: Option<String>,
    #[serde(rename = "signinUrl")]
    signin_url: Option<String>,
    #[serde(rename = "callbackUrl")]
    callback_url: Option<String>,
}

struct AntigravityAuthSession {
    api_base_url: String,
    auth_url: String,
}

/// PKCE verifier/challenge pair.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Anthropic OAuth authorization mode.
#[derive(Debug, Clone, Copy)]
pub enum AuthMode {
    /// Claude Pro/Max subscription (claude.ai)
    Max,
    /// API console (console.anthropic.com)
    Console,
}

/// Generate a PKCE verifier (64 random bytes, base64url-encoded) and S256 challenge.
pub fn generate_pkce() -> Pkce {
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);

    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);

    Pkce {
        verifier,
        challenge,
    }
}

/// Build the Anthropic authorization URL and return it with the PKCE verifier.
pub fn authorize_url(mode: AuthMode) -> (String, String) {
    let pkce = generate_pkce();

    let base = match mode {
        AuthMode::Max => "https://claude.ai/oauth/authorize",
        AuthMode::Console => "https://console.anthropic.com/oauth/authorize",
    };

    let url = format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        base,
        ANTHROPIC_CLIENT_ID,
        urlencoding::encode(ANTHROPIC_REDIRECT_URI),
        urlencoding::encode(ANTHROPIC_SCOPES),
        pkce.challenge,
        pkce.verifier,
    );

    (url, pkce.verifier)
}

/// Exchange an Anthropic authorization code for OAuth tokens.
///
/// The code from the browser is in the form `<code>#<state>`.
pub async fn exchange_code(code_with_state: &str, verifier: &str) -> Result<OAuthCredentials> {
    let (code, state) = code_with_state
        .split_once('#')
        .unwrap_or((code_with_state, ""));

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "code": code,
        "state": state,
        "grant_type": "authorization_code",
        "client_id": ANTHROPIC_CLIENT_ID,
        "redirect_uri": ANTHROPIC_REDIRECT_URI,
        "code_verifier": verifier,
    });

    let response = client
        .post(ANTHROPIC_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("failed to send token exchange request")?;

    let status = response.status();
    let text = response
        .text()
        .await
        .context("failed to read token exchange response")?;

    if !status.is_success() {
        anyhow::bail!("token exchange failed ({}): {}", status, text);
    }

    let json: AnthropicTokenResponse =
        serde_json::from_str(&text).context("failed to parse token exchange response")?;

    Ok(OAuthCredentials {
        access_token: json.access_token,
        refresh_token: json.refresh_token,
        expires_at: chrono::Utc::now().timestamp_millis() + json.expires_in * 1000,
    })
}

/// Path to the Anthropic OAuth credentials file within the instance directory.
pub fn credentials_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("anthropic_oauth.json")
}

/// Path to the Antigravity OAuth credentials file within the instance directory.
pub fn antigravity_credentials_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("antigravity_oauth.json")
}

/// Load stored Anthropic credentials from disk.
pub fn load_credentials(instance_dir: &Path) -> Result<Option<OAuthCredentials>> {
    let path = credentials_path(instance_dir);
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let credentials: OAuthCredentials =
        serde_json::from_str(&data).context("failed to parse auth.json")?;
    Ok(Some(credentials))
}

/// Load stored Antigravity credentials from disk.
pub fn load_antigravity_credentials(instance_dir: &Path) -> Result<Option<AntigravityCredentials>> {
    let path = antigravity_credentials_path(instance_dir);
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let credentials: AntigravityCredentials =
        serde_json::from_str(&data).context("failed to parse antigravity credentials")?;
    Ok(Some(credentials))
}

/// Save Anthropic credentials to disk with restricted permissions (0600).
pub fn save_credentials(instance_dir: &Path, credentials: &OAuthCredentials) -> Result<()> {
    let path = credentials_path(instance_dir);
    let data =
        serde_json::to_string_pretty(credentials).context("failed to serialize credentials")?;

    std::fs::write(&path, &data).with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }

    Ok(())
}

/// Save Antigravity credentials to disk with restricted permissions (0600).
pub fn save_antigravity_credentials(
    instance_dir: &Path,
    credentials: &AntigravityCredentials,
) -> Result<()> {
    let path = antigravity_credentials_path(instance_dir);
    let data = serde_json::to_string_pretty(credentials)
        .context("failed to serialize antigravity credentials")?;

    std::fs::write(&path, &data).with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }

    Ok(())
}

/// Run the interactive Anthropic OAuth login flow.
pub async fn login_interactive(instance_dir: &Path, mode: AuthMode) -> Result<OAuthCredentials> {
    let (url, verifier) = authorize_url(mode);

    eprintln!("Open this URL in your browser:\n");
    eprintln!("  {url}\n");

    if let Err(_error) = open::that(&url) {
        eprintln!("(Could not open browser automatically, please copy the URL above)");
    }

    eprintln!("After authorizing, paste the code here:");
    eprint!("> ");

    let mut code = String::new();
    std::io::stdin()
        .read_line(&mut code)
        .context("failed to read authorization code from stdin")?;
    let code = code.trim();

    if code.is_empty() {
        anyhow::bail!("no authorization code provided");
    }

    let credentials = exchange_code(code, &verifier)
        .await
        .context("failed to exchange authorization code")?;

    save_credentials(instance_dir, &credentials).context("failed to save credentials")?;

    eprintln!(
        "Login successful. Credentials saved to {}",
        credentials_path(instance_dir).display()
    );

    Ok(credentials)
}

fn normalize_base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

fn default_callback_url(base_url: &str, state: &str) -> String {
    format!("{}/callback/{}", normalize_base_url(base_url), state)
}

fn default_signin_url_prefix(base_url: &str) -> String {
    format!("{}/signin?url=", normalize_base_url(base_url))
}

fn antigravity_auth_base_urls() -> Vec<String> {
    let mut base_urls = Vec::new();

    if let Ok(override_url) = std::env::var("ANTIGRAVITY_AUTH_BASE_URL") {
        let override_url = override_url.trim();
        if !override_url.is_empty() {
            base_urls.push(normalize_base_url(override_url));
        }
    }

    // Backward-compatible override name used by earlier Spacebot versions.
    if let Ok(override_url) = std::env::var("ANTIGRAVITY_AUTH_API_BASE_URL") {
        let override_url = override_url.trim();
        if !override_url.is_empty() {
            base_urls.push(normalize_base_url(override_url));
        }
    }

    for default_url in ANTIGRAVITY_AUTH_BASE_URLS {
        let default_url = normalize_base_url(default_url);
        if !base_urls.contains(&default_url) {
            base_urls.push(default_url);
        }
    }

    base_urls
}

fn antigravity_poll_base_url(auth_base_url: &str) -> String {
    let mut poll_base_url = normalize_base_url(auth_base_url);
    poll_base_url = poll_base_url
        .replace("https://auth.", "https://api.")
        .replace("http://auth.", "http://api.");
    if poll_base_url.ends_with(".dev") {
        poll_base_url = format!(
            "{}.com",
            poll_base_url.trim_end_matches(".dev").trim_end_matches('/')
        );
    }
    poll_base_url
}

/// Start Antigravity auth and return the auth session metadata.
async fn antigravity_start_auth(state: &str) -> Result<AntigravityAuthSession> {
    let client = reqwest::Client::new();
    let mut errors = Vec::new();

    for auth_base_url in antigravity_auth_base_urls() {
        let callback_url = default_callback_url(&auth_base_url, state);
        let payload = serde_json::json!({
            "callback_url": callback_url,
            "state": state,
            "user_agent": "spacebot/1.0",
            "credentials": {
                "mode": "credentials"
            }
        });

        let response = match client
            .post(format!("{auth_base_url}/auth"))
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                errors.push(format!("{auth_base_url}: {error}"));
                continue;
            }
        };

        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read antigravity auth session response")?;

        if status.as_u16() != 201 {
            errors.push(format!(
                "{auth_base_url}: antigravity auth session failed ({status}): {body}"
            ));
            continue;
        }

        let parsed: AntigravityAuthStartResponse = serde_json::from_str(&body)
            .context("failed to parse antigravity auth session response")?;
        let signin_url_prefix = parsed
            .signin_url
            .or(parsed.url)
            .unwrap_or_else(|| default_signin_url_prefix(&auth_base_url));
        let callback = parsed
            .callback_url
            .unwrap_or_else(|| default_callback_url(&auth_base_url, state));
        let encoded_callback = STANDARD.encode(callback.as_bytes());
        let auth_url = format!(
            "{}{}",
            signin_url_prefix,
            urlencoding::encode(&encoded_callback)
        );

        return Ok(AntigravityAuthSession {
            api_base_url: antigravity_poll_base_url(&auth_base_url),
            auth_url,
        });
    }

    anyhow::bail!(
        "failed to create antigravity auth session; tried {} endpoint(s): {}",
        errors.len(),
        errors.join(" | ")
    );
}

async fn antigravity_poll_for_credentials(
    api_base_url: &str,
    state: &str,
) -> Result<serde_json::Value> {
    let client = reqwest::Client::new();
    for attempt in 1..=ANTIGRAVITY_MAX_POLL_ATTEMPTS {
        let response = client
            .get(format!("{}/auth/{state}", normalize_base_url(api_base_url)))
            .send()
            .await
            .with_context(|| format!("failed polling antigravity auth (attempt {attempt})"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read antigravity polling response")?;

        if status.as_u16() == 404 || status.as_u16() == 202 || body.trim() == "pending" {
            tokio::time::sleep(std::time::Duration::from_secs(
                ANTIGRAVITY_POLL_INTERVAL_SECS,
            ))
            .await;
            continue;
        }

        if !status.is_success() {
            anyhow::bail!("antigravity polling failed ({}): {}", status, body);
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&body).context("failed to parse antigravity polling response")?;
        return Ok(parsed);
    }

    anyhow::bail!(
        "timed out waiting for antigravity authorization after {} seconds",
        ANTIGRAVITY_MAX_POLL_ATTEMPTS * ANTIGRAVITY_POLL_INTERVAL_SECS as usize
    )
}

fn parse_antigravity_credentials(payload: &serde_json::Value) -> Result<AntigravityCredentials> {
    let credential_block = payload.get("credentials").unwrap_or(payload);
    let access_token = credential_block
        .get("access_token")
        .or_else(|| credential_block.get("accessToken"))
        .and_then(serde_json::Value::as_str)
        .context("antigravity credentials missing access_token")?
        .to_string();
    let refresh_token = credential_block
        .get("refresh_token")
        .or_else(|| credential_block.get("refreshToken"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let expires_at = credential_block
        .get("expiry_date")
        .or_else(|| credential_block.get("expires_at"))
        .or_else(|| credential_block.get("expiresAt"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() + 55 * 60 * 1000);
    let token_type = credential_block
        .get("token_type")
        .or_else(|| credential_block.get("tokenType"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let scope = credential_block
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let project_id = credential_block
        .get("project_id")
        .or_else(|| credential_block.get("projectId"))
        .or_else(|| payload.get("project_id"))
        .or_else(|| payload.get("projectId"))
        .and_then(serde_json::Value::as_str)
        .context("antigravity credentials missing project_id")?
        .to_string();

    Ok(AntigravityCredentials {
        access_token,
        refresh_token,
        expires_at,
        token_type,
        scope,
        project_id,
    })
}

/// Run interactive Antigravity OAuth login flow.
pub async fn antigravity_login_interactive(instance_dir: &Path) -> Result<AntigravityCredentials> {
    let state = uuid::Uuid::new_v4().to_string();
    let session = antigravity_start_auth(&state)
        .await
        .context("automatic OAuth start failed")?;

    eprintln!("Open this URL in your browser:\n");
    eprintln!("  {}\n", session.auth_url);
    eprintln!("Waiting for authorization callback...");

    if let Err(_error) = open::that(&session.auth_url) {
        eprintln!("(Could not open browser automatically, please copy the URL above)");
    }

    let payload = antigravity_poll_for_credentials(&session.api_base_url, &state)
        .await
        .context("automatic OAuth callback did not complete")?;
    let credentials = parse_antigravity_credentials(&payload)?;

    save_antigravity_credentials(instance_dir, &credentials)
        .context("failed to save antigravity credentials")?;

    eprintln!(
        "Login successful. Credentials saved to {}",
        antigravity_credentials_path(instance_dir).display()
    );

    Ok(credentials)
}
