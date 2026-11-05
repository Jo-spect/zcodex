//! xAI device authorization and provider-scoped, renewable Grok credentials.

mod storage;

use codex_config::types::AuthCredentialsStoreMode;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::Instant;

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";

/// Authorization instructions to show before polling for user approval.
#[derive(Deserialize)]
pub struct DeviceAuthorization {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
    #[serde(skip, default = "Instant::now")]
    started: Instant,
}

impl std::fmt::Display for DeviceAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Open {} and enter code: {}",
            self.verification_uri, self.user_code
        )
    }
}

#[derive(Serialize, Deserialize)]
struct Credentials {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    token_type: Option<String>,
    error: Option<String>,
}

/// Login and refresh client. Secrets are stored separately from ChatGPT credentials.
pub struct GrokLogin {
    client: reqwest::Client,
    storage: storage::Storage,
    device_url: String,
    token_url: String,
}

impl GrokLogin {
    pub fn new(
        home: PathBuf,
        mode: AuthCredentialsStoreMode,
        proxy: Option<&str>,
    ) -> io::Result<Self> {
        if mode == AuthCredentialsStoreMode::Ephemeral {
            return Err(io::Error::other(
                "Grok login requires persistent credential storage",
            ));
        }
        let client =
            crate::default_client::build_provider_client(proxy).map_err(io::Error::other)?;
        Ok(Self {
            client,
            storage: storage::Storage { home, mode },
            device_url: DEVICE_CODE_URL.into(),
            token_url: TOKEN_URL.into(),
        })
    }

    pub async fn begin(&self) -> io::Result<DeviceAuthorization> {
        let response = self
            .client
            .post(&self.device_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(/*secs*/ 30))
            .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
            .send()
            .await
            .map_err(|_| io::Error::other("Cannot reach xAI device authorization endpoint"))?;
        if !response.status().is_success() {
            return Err(io::Error::other(format!(
                "xAI device authorization failed (HTTP {})",
                response.status()
            )));
        }
        let device: DeviceAuthorization = response
            .json()
            .await
            .map_err(|_| io::Error::other("Invalid xAI device authorization response"))?;
        if device.device_code.is_empty()
            || device.user_code.is_empty()
            || device.expires_in == 0
            || !device.verification_uri.starts_with("https://")
        {
            return Err(io::Error::other(
                "Incomplete xAI device authorization response",
            ));
        }
        Ok(device)
    }

    pub async fn complete(&self, device: DeviceAuthorization) -> io::Result<()> {
        let deadline = device.started + Duration::from_secs(device.expires_in.min(3600));
        let mut interval = Duration::from_secs(
            device
                .interval
                .unwrap_or(5)
                .max(1)
                .min(device.expires_in.min(3600)),
        );
        loop {
            let reply = tokio::time::timeout_at(deadline, async {
                tokio::time::sleep(interval).await;
                self.token(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", &device.device_code),
                ])
                .await
            })
            .await
            .map_err(|_| {
                io::Error::other("Grok device code expired; run `codex login grok` again")
            })??;
            match reply.error.as_deref() {
                Some("authorization_pending") => continue,
                Some("slow_down") => {
                    interval += Duration::from_secs(/*secs*/ 5);
                    continue;
                }
                Some("access_denied") => return Err(io::Error::other("Grok authorization denied")),
                Some("expired_token") => return Err(io::Error::other("Grok device code expired")),
                Some(_) => return Err(io::Error::other("Grok authorization failed")),
                None => {}
            }
            let credentials = reply.credentials(/*previous_refresh_token*/ None)?;
            let _lock = self.storage.lock().await?;
            return self.storage.save(&credentials);
        }
    }

    /// Returns the current bearer, refreshing and atomically saving rotated tokens when needed.
    pub async fn access_token(&self) -> io::Result<String> {
        let _lock = self.storage.lock().await?;
        let credentials = self.storage.load()?;
        if credentials.expires_at > chrono::Utc::now().timestamp().saturating_add(60)
            && !credentials.access_token.is_empty()
        {
            return Ok(credentials.access_token);
        }
        let refresh = credentials
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| io::Error::other("Grok login expired; run `codex login grok` again"))?;
        let reply = self
            .token(&[("grant_type", "refresh_token"), ("refresh_token", refresh)])
            .await?;
        if reply.error.is_some() {
            return Err(io::Error::other(
                "Grok token refresh rejected; run `codex login grok` again",
            ));
        }
        let credentials = reply.credentials(Some(refresh))?;
        self.storage.save(&credentials)?;
        Ok(credentials.access_token)
    }

    async fn token(&self, fields: &[(&str, &str)]) -> io::Result<TokenReply> {
        let mut fields = fields.to_vec();
        fields.push(("client_id", CLIENT_ID));
        let response = self
            .client
            .post(&self.token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(/*secs*/ 30))
            .form(&fields)
            .send()
            .await
            .map_err(|_| io::Error::other("Cannot reach xAI token endpoint"))?;
        let status = response.status();
        let reply: TokenReply = response
            .json()
            .await
            .map_err(|_| io::Error::other(format!("Invalid xAI token response (HTTP {status})")))?;
        if !status.is_success() && reply.error.is_none() {
            return Err(io::Error::other(format!(
                "xAI token request failed (HTTP {status})"
            )));
        }
        Ok(reply)
    }
}

impl TokenReply {
    fn credentials(self, previous_refresh_token: Option<&str>) -> io::Result<Credentials> {
        if self
            .token_type
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
        {
            return Err(io::Error::other("xAI returned an unsupported token type"));
        }
        let access_token = self
            .access_token
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| io::Error::other("xAI returned an empty access token"))?;
        let expires_in = self
            .expires_in
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| io::Error::other("xAI token response omitted a valid expires_in"))?;
        Ok(Credentials {
            access_token,
            refresh_token: self
                .refresh_token
                .filter(|token| !token.is_empty())
                .or_else(|| previous_refresh_token.map(str::to_owned)),
            expires_at: chrono::Utc::now()
                .timestamp()
                .saturating_add(i64::try_from(expires_in).unwrap_or(i64::MAX)),
        })
    }
}

#[cfg(test)]
#[path = "grok_tests.rs"]
mod tests;
