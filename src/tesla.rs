use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const OWNER_API: &str = "https://owner-api.teslamotors.com";
const AUTH_API: &str = "https://auth.tesla.com/oauth2/v3/token";
const STREAM_URL: &str = "wss://streaming.vn.teslamotors.com/streaming/";

fn owner_api() -> String {
    std::env::var("TESLA_API_HOST").unwrap_or_else(|_| OWNER_API.to_string())
}

fn auth_api() -> String {
    std::env::var("TESLA_AUTH_URL").unwrap_or_else(|_| AUTH_API.to_string())
}

pub fn stream_endpoint() -> String {
    std::env::var("TESLA_WSS_HOST").unwrap_or_else(|_| STREAM_URL.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

#[derive(Clone)]
pub struct Tesla {
    http: Client,
    tokens: Tokens,
}

impl Tesla {
    pub fn new(tokens: Tokens) -> Result<Self> {
        let http = Client::builder()
            .user_agent("teslamate-rs/0.1")
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { http, tokens })
    }

    pub async fn from_refresh_token(refresh_token: &str) -> Result<Self> {
        let http = Client::builder()
            .user_agent("teslamate-rs/0.1")
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let tokens = refresh_with(&http, refresh_token).await?;
        Ok(Self { http, tokens })
    }

    pub fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    pub async fn ensure_fresh(&mut self) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        if self.tokens.expires_at.saturating_sub(now) > 120 {
            return Ok(());
        }
        self.tokens = refresh_with(&self.http, &self.tokens.refresh_token).await?;
        Ok(())
    }

    pub async fn products(&mut self) -> Result<Vec<Value>> {
        self.ensure_fresh().await?;
        let v = self
            .get("/api/1/products")
            .await?
            .get("response")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub async fn vehicle_data(&mut self, id: i64) -> Result<Value> {
        self.ensure_fresh().await?;
        let path = format!(
            "/api/1/vehicles/{id}/vehicle_data?endpoints=charge_state;climate_state;closures_state;drive_state;gui_settings;location_data;vehicle_config;vehicle_state;vehicle_data_combo"
        );
        Ok(self
            .get(&path)
            .await?
            .get("response")
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub async fn vehicle(&mut self, id: i64) -> Result<Value> {
        self.ensure_fresh().await?;
        Ok(self
            .get(&format!("/api/1/vehicles/{id}"))
            .await?
            .get("response")
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", owner_api());
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.tokens.access_token)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("GET {path} -> {status}: {body}");
        }
        serde_json::from_str(&body).with_context(|| format!("decode {path}"))
    }

    pub fn stream_url() -> String {
        stream_endpoint()
    }
}

async fn refresh_with(http: &Client, refresh_token: &str) -> Result<Tokens> {
    let resp = http
        .post(auth_api())
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": "ownerapi",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .context("token refresh")?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("token refresh {status}: {body}");
    }
    let access = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .context("access_token missing")?
        .to_string();
    let refresh = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or(refresh_token)
        .to_string();
    let expires_in = body.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(28800);
    Ok(Tokens {
        access_token: access,
        refresh_token: refresh,
        expires_at: chrono::Utc::now().timestamp() + expires_in,
    })
}

pub fn f64_field(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_f64()
        .or_else(|| cur.as_i64().map(|i| i as f64))
        .or_else(|| cur.as_u64().map(|i| i as f64))
}

pub fn i64_field(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_i64()
        .or_else(|| cur.as_u64().map(|u| u as i64))
        .or_else(|| cur.as_f64().map(|f| f as i64))
}

pub fn str_field<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str()
}

pub fn bool_field(v: &Value, path: &[&str]) -> Option<bool> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_bool()
}
