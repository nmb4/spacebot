//! OAuth authentication for provider-backed subscriptions.

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const ANTHROPIC_SCOPES: &str = "org:create_api_key user:profile user:inference";

const ANTIGRAVITY_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const ANTIGRAVITY_OAUTH_CLIENT_ID_ENV: &str = "ANTIGRAVITY_OAUTH_CLIENT_ID";
const ANTIGRAVITY_OAUTH_CLIENT_SECRET_ENV: &str = "ANTIGRAVITY_OAUTH_CLIENT_SECRET";
const ANTIGRAVITY_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const ANTIGRAVITY_REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
const ANTIGRAVITY_CALLBACK_WAIT_TIMEOUT_SECS: u64 = 300;
const ANTIGRAVITY_DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";
const ANTIGRAVITY_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

fn antigravity_oauth_client_id() -> Result<String> {
    std::env::var(ANTIGRAVITY_OAUTH_CLIENT_ID_ENV).with_context(|| {
        format!(
            "missing {} (required for Antigravity OAuth login)",
            ANTIGRAVITY_OAUTH_CLIENT_ID_ENV
        )
    })
}

fn antigravity_oauth_client_secret() -> Result<String> {
    std::env::var(ANTIGRAVITY_OAUTH_CLIENT_SECRET_ENV).with_context(|| {
        format!(
            "missing {} (required for Antigravity OAuth login)",
            ANTIGRAVITY_OAUTH_CLIENT_SECRET_ENV
        )
    })
}

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
        let client_id = antigravity_oauth_client_id()?;
        let client_secret = antigravity_oauth_client_secret()?;

        let client = reqwest::Client::new();
        let body = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
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
struct AntigravityCodeExchangeResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    token_type: Option<String>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityLoadCodeAssistResponse {
    #[serde(rename = "cloudaicompanionProject")]
    cloudaicompanion_project: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct AntigravityCallbackResult {
    code: String,
    state: String,
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

fn build_antigravity_auth_url(state: &str, code_challenge: &str) -> Result<String> {
    let client_id = antigravity_oauth_client_id()?;
    let mut params = Vec::new();
    params.push(format!("client_id={}", urlencoding::encode(&client_id)));
    params.push("response_type=code".to_string());
    params.push(format!(
        "redirect_uri={}",
        urlencoding::encode(ANTIGRAVITY_REDIRECT_URI)
    ));
    params.push(format!(
        "scope={}",
        urlencoding::encode(&ANTIGRAVITY_SCOPES.join(" "))
    ));
    params.push(format!(
        "code_challenge={}",
        urlencoding::encode(code_challenge)
    ));
    params.push("code_challenge_method=S256".to_string());
    params.push(format!("state={}", urlencoding::encode(state)));
    params.push("access_type=offline".to_string());
    params.push("prompt=consent".to_string());
    Ok(format!("{ANTIGRAVITY_AUTH_URL}?{}", params.join("&")))
}

fn parse_query_string(value: &str) -> HashMap<String, String> {
    let mut pairs = HashMap::new();
    for pair in value.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = urlencoding::decode(key)
            .map(|decoded| decoded.into_owned())
            .unwrap_or_else(|_| key.to_string());
        let parsed_value = urlencoding::decode(raw_value)
            .map(|decoded| decoded.into_owned())
            .unwrap_or_else(|_| raw_value.to_string());
        pairs.insert(key, parsed_value);
    }
    pairs
}

fn parse_auth_code_input(input: &str) -> Option<AntigravityCallbackResult> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(query) = trimmed
        .split_once('?')
        .map(|(_prefix, query)| query)
        .or_else(|| trimmed.strip_prefix('?'))
    {
        let pairs = parse_query_string(query);
        if let Some(code) = pairs.get("code") {
            return Some(AntigravityCallbackResult {
                code: code.to_string(),
                state: pairs.get("state").cloned().unwrap_or_default(),
            });
        }
    }

    if let Some((code, state)) = trimmed.split_once('#') {
        if !code.is_empty() {
            return Some(AntigravityCallbackResult {
                code: code.to_string(),
                state: state.to_string(),
            });
        }
    }

    Some(AntigravityCallbackResult {
        code: trimmed.to_string(),
        state: String::new(),
    })
}

fn write_http_response(stream: &mut TcpStream, status_line: &str, body: &str) {
    let response = format!(
        "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn start_antigravity_callback_listener() -> Result<mpsc::Receiver<AntigravityCallbackResult>> {
    let listener = TcpListener::bind(("127.0.0.1", 51121))
        .context("failed to bind local OAuth callback server on 127.0.0.1:51121")?;
    listener
        .set_nonblocking(true)
        .context("failed to configure callback listener")?;

    let (sender, receiver) = mpsc::channel::<AntigravityCallbackResult>();

    std::thread::spawn(move || {
        let deadline =
            std::time::Instant::now() + Duration::from_secs(ANTIGRAVITY_CALLBACK_WAIT_TIMEOUT_SECS);
        loop {
            if std::time::Instant::now() >= deadline {
                break;
            }

            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    let mut buffer = [0_u8; 8192];
                    let read_bytes = match stream.read(&mut buffer) {
                        Ok(size) => size,
                        Err(_) => continue,
                    };
                    if read_bytes == 0 {
                        continue;
                    }

                    let request = String::from_utf8_lossy(&buffer[..read_bytes]);
                    let first_line = request.lines().next().unwrap_or_default();
                    let path = first_line.split_whitespace().nth(1).unwrap_or_default();

                    if !path.starts_with("/oauth-callback") {
                        write_http_response(
                            &mut stream,
                            "HTTP/1.1 404 Not Found",
                            "<html><body><h1>Not Found</h1></body></html>",
                        );
                        continue;
                    }

                    let query = path
                        .split_once('?')
                        .map(|(_prefix, query)| query)
                        .unwrap_or_default();
                    let pairs = parse_query_string(query);

                    if let Some(error) = pairs.get("error") {
                        write_http_response(
                            &mut stream,
                            "HTTP/1.1 400 Bad Request",
                            &format!(
                                "<html><body><h1>Authentication Failed</h1><p>Error: {error}</p><p>You can close this window.</p></body></html>"
                            ),
                        );
                        continue;
                    }

                    let code = pairs.get("code").cloned();
                    let state = pairs.get("state").cloned();
                    match (code, state) {
                        (Some(code), Some(state)) => {
                            write_http_response(
                                &mut stream,
                                "HTTP/1.1 200 OK",
                                "<html><body><h1>Authentication Successful</h1><p>You can close this window and return to the terminal.</p></body></html>",
                            );
                            let _ = sender.send(AntigravityCallbackResult { code, state });
                            break;
                        }
                        _ => {
                            write_http_response(
                                &mut stream,
                                "HTTP/1.1 400 Bad Request",
                                "<html><body><h1>Authentication Failed</h1><p>Missing code or state parameter.</p></body></html>",
                            );
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
    });

    Ok(receiver)
}

async fn exchange_antigravity_code(
    code: &str,
    verifier: &str,
) -> Result<AntigravityCodeExchangeResponse> {
    let client_id = antigravity_oauth_client_id()?;
    let client_secret = antigravity_oauth_client_secret()?;
    let client = reqwest::Client::new();
    let body = [
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", ANTIGRAVITY_REDIRECT_URI),
        ("code_verifier", verifier),
    ];

    let response = client
        .post(ANTIGRAVITY_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&body)
        .send()
        .await
        .context("failed to send antigravity token exchange request")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read antigravity token exchange response")?;

    if !status.is_success() {
        anyhow::bail!("antigravity token exchange failed ({}): {}", status, body);
    }

    let parsed: AntigravityCodeExchangeResponse = serde_json::from_str(&body)
        .context("failed to parse antigravity token exchange response")?;
    Ok(parsed)
}

fn extract_project_id(payload: &serde_json::Value) -> Option<String> {
    if let Some(project_id) = payload.as_str() {
        if !project_id.is_empty() {
            return Some(project_id.to_string());
        }
    }
    if let Some(project_id) = payload.get("id").and_then(serde_json::Value::as_str) {
        if !project_id.is_empty() {
            return Some(project_id.to_string());
        }
    }
    None
}

async fn discover_antigravity_project(access_token: &str) -> String {
    let client = reqwest::Client::new();
    let endpoints = [
        "https://cloudcode-pa.googleapis.com",
        "https://daily-cloudcode-pa.sandbox.googleapis.com",
    ];

    for endpoint in endpoints {
        let response = match client
            .post(format!("{endpoint}/v1internal:loadCodeAssist"))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", "google-api-nodejs-client/9.15.1")
            .header("X-Goog-Api-Client", "google-cloud-sdk vscode_cloudshelleditor/0.1")
            .header(
                "Client-Metadata",
                r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#,
            )
            .json(&serde_json::json!({
                "metadata": {
                    "ideType": "IDE_UNSPECIFIED",
                    "platform": "PLATFORM_UNSPECIFIED",
                    "pluginType": "GEMINI"
                }
            }))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => continue,
        };

        if !response.status().is_success() {
            continue;
        }

        let body = match response.text().await {
            Ok(body) => body,
            Err(_) => continue,
        };

        let parsed: AntigravityLoadCodeAssistResponse = match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if let Some(project_value) = parsed.cloudaicompanion_project
            && let Some(project_id) = extract_project_id(&project_value)
        {
            return project_id;
        }
    }

    ANTIGRAVITY_DEFAULT_PROJECT_ID.to_string()
}

fn read_line_trimmed(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    let mut value = String::new();
    std::io::stdin()
        .read_line(&mut value)
        .context("failed to read input from stdin")?;
    Ok(value.trim().to_string())
}

/// Run interactive Antigravity OAuth login flow.
pub async fn antigravity_login_interactive(instance_dir: &Path) -> Result<AntigravityCredentials> {
    let pkce = generate_pkce();
    let auth_url = build_antigravity_auth_url(&pkce.verifier, &pkce.challenge)?;
    let callback_receiver = start_antigravity_callback_listener().ok();

    eprintln!("Open this URL in your browser:\n");
    eprintln!("  {auth_url}\n");
    eprintln!("Sign in with your Google account.");
    eprintln!();

    if let Err(_error) = open::that(&auth_url) {
        eprintln!("(Could not open browser automatically, please copy the URL above)");
    }

    let callback_result = if let Some(receiver) = callback_receiver {
        eprintln!(
            "Waiting up to {} seconds for callback on {} ...",
            ANTIGRAVITY_CALLBACK_WAIT_TIMEOUT_SECS, ANTIGRAVITY_REDIRECT_URI
        );
        match tokio::task::spawn_blocking(move || {
            receiver.recv_timeout(Duration::from_secs(ANTIGRAVITY_CALLBACK_WAIT_TIMEOUT_SECS))
        })
        .await
        {
            Ok(Ok(result)) => Some(result),
            Ok(Err(_)) | Err(_) => None,
        }
    } else {
        None
    };

    let callback_result = if let Some(result) = callback_result {
        result
    } else {
        eprintln!("Automatic callback was not received.");
        eprintln!("Paste the full redirect URL from your browser (or paste just the code):");
        let input = read_line_trimmed("> ")?;
        parse_auth_code_input(&input).context(
            "failed to parse authorization response; expected URL containing code and state",
        )?
    };

    if !callback_result.state.is_empty() && callback_result.state != pkce.verifier {
        anyhow::bail!("OAuth state mismatch - authentication aborted");
    }

    eprintln!("Exchanging authorization code for tokens...");
    let token_response = exchange_antigravity_code(&callback_result.code, &pkce.verifier).await?;

    let refresh_token = token_response
        .refresh_token
        .context("no refresh token returned; retry login and ensure consent is granted")?;
    let expires_at =
        chrono::Utc::now().timestamp_millis() + token_response.expires_in * 1000 - 5 * 60 * 1000;
    let project_id = discover_antigravity_project(&token_response.access_token).await;

    let credentials = AntigravityCredentials {
        access_token: token_response.access_token,
        refresh_token: Some(refresh_token),
        expires_at,
        token_type: token_response.token_type,
        scope: token_response.scope,
        project_id,
    };

    save_antigravity_credentials(instance_dir, &credentials)
        .context("failed to save antigravity credentials")?;

    eprintln!(
        "Login successful. Credentials saved to {}",
        antigravity_credentials_path(instance_dir).display()
    );

    Ok(credentials)
}
