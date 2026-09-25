use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const OWNER_API: &str = "https://owner-api.teslamotors.com";
const AUTH_API: &str = "https://auth.tesla.com/oauth2/v3/token";
const STREAM_URL: &str = "wss://streaming.vn.teslamotors.com/streaming/";
const CHARGING_GQL: &str =
    "https://akamai-apigateway-charging-ownership.tesla.com/graphql";
const CHARGING_HISTORY: &str = "https://ownership.tesla.com/mobile-app/charging/history";
const TESLA_APP_UA: &str = "TeslaApp/4.40.5";

const CHARGING_HISTORY_QUERY: &str = r#"
query getChargingHistoryV2($pageNumber: Int!, $sortBy: String, $sortOrder: SortByEnum) {
  me {
    charging {
      historyV2(pageNumber: $pageNumber, sortBy: $sortBy, sortOrder: $sortOrder) {
        data { ...SparkHistoryItemFragment }
        totalResults
        hasMoreData
        pageNumber
      }
    }
  }
}
fragment SparkHistoryItemFragment on SparkHistoryItem {
  countryCode
  programType
  billingType
  vin
  credit { distance distanceUnit }
  chargingPackage { distance distanceUnit energyApplied }
  invoices { fileName contentId invoiceType }
  chargeSessionId
  siteLocationName
  chargeStartDateTime
  chargeStopDateTime
  unlatchDateTime
  fees { ...SparkHistoryFeeFragment }
  vehicleMakeType
  sessionId
  surveyCompleted
  surveyType
  postId
  cabinetId
  din
}
fragment SparkHistoryFeeFragment on SparkHistoryFee {
  sessionFeeId
  feeType
  payorUid
  amountDue
  currencyCode
  pricingType
  usageBase
  usageTier1
  usageTier2
  usageTier3
  usageTier4
  rateBase
  rateTier1
  rateTier2
  rateTier3
  rateTier4
  uom
  isPaid
  uid
  totalBase
  totalDue
  netDue
  status
}
"#;

fn owner_api() -> String {
    std::env::var("TESLA_API_HOST").unwrap_or_else(|_| OWNER_API.to_string())
}

fn charging_gql() -> String {
    std::env::var("TESLA_CHARGING_GQL").unwrap_or_else(|_| {
        let host = owner_api();
        if host.contains("teslamotors.com") || host.contains("tesla.com") {
            CHARGING_GQL.to_string()
        } else {
            format!("{host}/graphql")
        }
    })
}

fn charging_history() -> String {
    std::env::var("TESLA_CHARGING_HISTORY").unwrap_or_else(|_| {
        let host = owner_api();
        if host.contains("teslamotors.com") || host.contains("tesla.com") {
            CHARGING_HISTORY.to_string()
        } else {
            format!("{host}/mobile-app/charging/history")
        }
    })
}

fn device_country() -> String {
    std::env::var("TESLAMATE_RS_DEVICE_COUNTRY").unwrap_or_else(|_| "GB".into())
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

fn build_http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent("teslamate-rs/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn insecure_tesla_ok() -> bool {
    matches!(
        std::env::var("TESLAMATE_RS_ALLOW_INSECURE_TESLA")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn host_allowed(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h == "owner-api.teslamotors.com"
        || h == "auth.tesla.com"
        || h == "streaming.vn.teslamotors.com"
        || h == "akamai-apigateway-charging-ownership.tesla.com"
        || h == "ownership.tesla.com"
        || h.ends_with(".teslamotors.com")
        || h.ends_with(".tesla.com")
    {
        return true;
    }
    if insecure_tesla_ok() && (h == "localhost" || h == "127.0.0.1" || h == "::1") {
        return true;
    }
    false
}

pub fn assert_tesla_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).with_context(|| format!("parse url"))?;
    let scheme = parsed.scheme();
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("url missing host"))?;
    match scheme {
        "https" | "wss" => {}
        "http" | "ws" if insecure_tesla_ok() && host_allowed(host) => {}
        _ => bail!("refusing non-TLS Tesla URL ({scheme}://{host})"),
    }
    if !host_allowed(host) {
        bail!("refusing Tesla API host {host:?}; set TESLAMATE_RS_ALLOW_INSECURE_TESLA=1 for local mocks");
    }
    Ok(())
}

impl Tesla {
    pub fn new(tokens: Tokens) -> Result<Self> {
        let http = build_http_client()?;
        Ok(Self { http, tokens })
    }

    pub async fn from_refresh_token(refresh_token: &str) -> Result<Self> {
        let http = build_http_client()?;
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
        self.force_refresh().await
    }

    pub async fn force_refresh(&mut self) -> Result<()> {
        self.tokens = refresh_with(&self.http, &self.tokens.refresh_token).await?;
        Ok(())
    }

    pub async fn products(&mut self) -> Result<Vec<Value>> {
        self.ensure_fresh().await?;
        self.list_path("/api/1/products").await
    }

    pub async fn vehicles(&mut self) -> Result<Vec<Value>> {
        self.ensure_fresh().await?;
        self.list_path("/api/1/vehicles").await
    }

    async fn list_path(&self, path: &str) -> Result<Vec<Value>> {
        let v = self
            .get(path)
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

    /// Nearby Superchargers / destination chargers. Vehicle must already be online.
    pub async fn nearby_charging_sites(&mut self, id: i64) -> Result<Value> {
        self.ensure_fresh().await?;
        Ok(self
            .get(&format!("/api/1/vehicles/{id}/nearby_charging_sites"))
            .await?
            .get("response")
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Account Supercharger invoices. Does not call vehicle_data (will not wake the car).
    pub async fn charging_history_page(&mut self, vin: &str, page: i64) -> Result<Value> {
        self.ensure_fresh().await?;
        match self.charging_history_graphql(vin, page).await {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::debug!("charging history graphql: {e:#}");
                self.charging_history_get(vin, page).await
            }
        }
    }

    async fn charging_history_graphql(&self, vin: &str, page: i64) -> Result<Value> {
        let country = device_country();
        let url = format!(
            "{}?deviceLanguage=en&deviceCountry={country}&ttpLocale=en_GB&vin={vin}&operationName=getChargingHistoryV2",
            charging_gql()
        );
        let body = serde_json::json!({
            "query": CHARGING_HISTORY_QUERY,
            "variables": {
                "pageNumber": page,
                "sortBy": "start_datetime",
                "sortOrder": "DESC",
            },
            "operationName": "getChargingHistoryV2",
        });
        self.post_url(&url, &body).await
    }

    async fn charging_history_get(&self, vin: &str, page: i64) -> Result<Value> {
        let country = device_country();
        let url = format!(
            "{}?vin={vin}&deviceLanguage=en&deviceCountry={country}&httpLocale=en_GB&operationName=getChargingHistoryV2&pageNumber={page}",
            charging_history()
        );
        self.get_url(&url).await
    }

    pub async fn users_me(&mut self) -> Result<Value> {
        self.ensure_fresh().await?;
        Ok(self
            .get("/api/1/users/me")
            .await?
            .get("response")
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub async fn mobile_enabled(&mut self, id: i64) -> Result<bool> {
        self.ensure_fresh().await?;
        let v = self
            .get(&format!("/api/1/vehicles/{id}/mobile_enabled"))
            .await?;
        Ok(v.get("response").and_then(|x| x.as_bool()).unwrap_or(false))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let label = redact_path(path);
        let url = format!("{}{path}", owner_api());
        assert_tesla_url(&url)?;
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.tokens.access_token)
            .header("x-tesla-user-agent", TESLA_APP_UA)
            .send()
            .await
            .with_context(|| format!("GET {label}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_redirection() {
            bail!("GET {label} redirected ({status}); refusing to follow with bearer token");
        }
        if !status.is_success() {
            bail!("GET {label} -> {status}");
        }
        serde_json::from_str(&body).with_context(|| format!("decode {label}"))
    }

    async fn get_url(&self, url: &str) -> Result<Value> {
        assert_tesla_url(url)?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.tokens.access_token)
            .header("x-tesla-user-agent", TESLA_APP_UA)
            .send()
            .await
            .context("GET charging-history")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_redirection() {
            bail!("GET charging-history redirected ({status}); refusing to follow with bearer token");
        }
        if !status.is_success() {
            bail!("GET charging-history -> {status}");
        }
        serde_json::from_str(&body).context("decode charging-history")
    }

    async fn post_url(&self, url: &str, body: &Value) -> Result<Value> {
        assert_tesla_url(url)?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.tokens.access_token)
            .header("x-tesla-user-agent", TESLA_APP_UA)
            .json(body)
            .send()
            .await
            .context("POST charging-history")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_redirection() {
            bail!("POST charging-history redirected ({status}); refusing to follow with bearer token");
        }
        if !status.is_success() {
            bail!("POST charging-history -> {status}");
        }
        let v: Value = serde_json::from_str(&text).context("decode charging-history graphql")?;
        if v.get("errors").is_some() {
            bail!("POST charging-history graphql errors");
        }
        Ok(v)
    }

    pub fn stream_url() -> String {
        let url = stream_endpoint();
        if let Err(e) = assert_tesla_url(&url) {
            tracing::error!("invalid TESLA_WSS_HOST: {e:#}");
        }
        url
    }
}

async fn refresh_with(http: &Client, refresh_token: &str) -> Result<Tokens> {
    let url = auth_api();
    assert_tesla_url(&url)?;
    let resp = http
        .post(&url)
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
    if status.is_redirection() {
        bail!("token refresh redirected ({status}); refusing to follow");
    }
    if !status.is_success() {
        bail!("token refresh {status}");
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

fn redact_path(path: &str) -> String {
    let (base, query) = path.split_once('?').unwrap_or((path, ""));
    let redacted = base
        .split('/')
        .map(|seg| {
            if !seg.is_empty() && seg.len() >= 4 && seg.chars().all(|c| c.is_ascii_digit()) {
                "{id}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    if query.is_empty() {
        redacted
    } else {
        format!("{redacted}?{query}")
    }
}

pub fn bool_field(v: &Value, path: &[&str]) -> Option<bool> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_bool()
}
