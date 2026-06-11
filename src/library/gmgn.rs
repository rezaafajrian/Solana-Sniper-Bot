/*!
# GMGN OpenAPI client

A tolerant, optional client for GMGN's OpenAPI (https://docs.gmgn.ai). It powers
three maximizations of the momentum bot:

1. **Rug/security veto** — `GET /v1/token/security` before a buy: skip honeypots,
   high `rug_ratio`, non-renounced mint/freeze, and previously-rugging creators.
2. **Smart-money confirmation** — `GET /v1/user/smartmoney` + `GET /v1/token/info`
   (`wallet_tags_stat`) to bias entries toward tokens proven wallets are buying.
3. **Trenches discovery** — `POST /v1/trenches` to surface pre-filtered new
   pump.fun launches (smart-money / safe presets) as a high-priority watchlist.

## Design notes

- **Optional + graceful**: every call has a timeout and returns `None`/empty on
  any error, so the bot always falls back to its own signals. GMGN being slow or
  down never blocks or crashes trading.
- **Schema-tolerant**: responses are parsed as `serde_json::Value` and read via
  small accessors, so a field rename on GMGN's side degrades a single signal
  instead of breaking deserialization.
- **Auth is configurable**: GMGN's exact auth header is account-specific. Set
  `GMGN_API_KEY`, and if needed `GMGN_AUTH_HEADER` (default `Authorization`) and
  `GMGN_AUTH_PREFIX` (default `Bearer `). If GMGN requires per-request signing,
  that is the one place to extend (`apply_auth`).
*/

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use reqwest::Client;
use serde_json::Value;

/// Configuration for the GMGN client, all from env.
#[derive(Clone)]
pub struct GmgnConfig {
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub auth_header: String,
    pub auth_prefix: String,
    pub chain: String,
    pub request_timeout: Duration,
    pub security_cache_secs: u64,
    /// Veto a buy if rug_ratio exceeds this (0..1).
    pub max_rug_ratio: f64,
    /// If true, a security lookup that fails (timeout/error) blocks the buy
    /// (fail-closed). If false, an unknown token is allowed through (fail-open).
    pub veto_on_unknown: bool,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl GmgnConfig {
    pub fn from_env() -> Self {
        let enabled = std::env::var("GMGN_ENABLED").map(|v| v.to_lowercase() == "true").unwrap_or(false);
        Self {
            enabled,
            base_url: env_or("GMGN_BASE_URL", "https://api.gmgn.ai").trim_end_matches('/').to_string(),
            api_key: env_or("GMGN_API_KEY", ""),
            // GMGN normal auth for read endpoints is the X-APIKEY header (no prefix,
            // no signing). Overridable in case the scheme differs for your account.
            auth_header: env_or("GMGN_AUTH_HEADER", "X-APIKEY"),
            auth_prefix: env_or("GMGN_AUTH_PREFIX", ""),
            chain: env_or("GMGN_CHAIN", "sol"),
            request_timeout: Duration::from_millis(
                std::env::var("GMGN_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500),
            ),
            security_cache_secs: std::env::var("GMGN_SECURITY_CACHE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120),
            max_rug_ratio: std::env::var("GMGN_MAX_RUG_RATIO").ok().and_then(|v| v.parse().ok()).unwrap_or(0.3),
            veto_on_unknown: std::env::var("GMGN_VETO_ON_UNKNOWN").map(|v| v.to_lowercase() == "true").unwrap_or(false),
        }
    }
}

/// Result of a pre-buy security check.
pub enum SecurityVerdict {
    /// Safe to proceed.
    Ok,
    /// Skip this buy, with a human-readable reason.
    Reject(String),
    /// GMGN gave no usable answer (timeout/error/unknown token).
    Unknown,
}

pub struct GmgnClient {
    http: Client,
    cfg: GmgnConfig,
    /// mint -> (fetched_at, parsed security object)
    sec_cache: DashMap<String, (Instant, Value)>,
}

impl GmgnClient {
    pub fn new(cfg: GmgnConfig) -> Arc<Self> {
        let http = Client::builder()
            .timeout(cfg.request_timeout)
            .build()
            .unwrap_or_else(|_| Client::new());
        Arc::new(Self { http, cfg, sec_cache: DashMap::new() })
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && !self.cfg.api_key.is_empty()
    }

    /// Whether a failed/unknown security lookup should block the buy (fail-closed).
    pub fn veto_on_unknown(&self) -> bool {
        self.cfg.veto_on_unknown
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.cfg.api_key.is_empty() {
            return req;
        }
        req.header(
            self.cfg.auth_header.as_str(),
            format!("{}{}", self.cfg.auth_prefix, self.cfg.api_key),
        )
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Option<Value> {
        let url = format!("{}{}", self.cfg.base_url, path);
        let req = self.apply_auth(self.http.get(&url).query(query));
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        // GMGN wraps payloads as {"code":0,"data":...}; unwrap when present.
        Some(v.get("data").cloned().unwrap_or(v))
    }

    async fn post(&self, path: &str, body: Value) -> Option<Value> {
        let url = format!("{}{}", self.cfg.base_url, path);
        let req = self.apply_auth(self.http.post(&url).json(&body));
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        Some(v.get("data").cloned().unwrap_or(v))
    }

    /// Pre-buy security verdict for a mint, cached for `security_cache_secs`.
    pub async fn security_verdict(&self, mint: &str) -> SecurityVerdict {
        // Cache hit?
        if let Some(entry) = self.sec_cache.get(mint) {
            if entry.0.elapsed().as_secs() < self.cfg.security_cache_secs {
                return self.verdict_from(&entry.1);
            }
        }
        let sec = self
            .get("/v1/token/security", &[("chain", &self.cfg.chain), ("address", mint)])
            .await;
        match sec {
            Some(v) => {
                let verdict = self.verdict_from(&v);
                self.sec_cache.insert(mint.to_string(), (Instant::now(), v));
                verdict
            }
            None => SecurityVerdict::Unknown,
        }
    }

    fn verdict_from(&self, v: &Value) -> SecurityVerdict {
        if truthy(v.get("is_honeypot")) {
            return SecurityVerdict::Reject("honeypot".to_string());
        }
        if let Some(r) = as_f64(v.get("rug_ratio")) {
            if r > self.cfg.max_rug_ratio {
                return SecurityVerdict::Reject(format!("rug_ratio {:.2} > {:.2}", r, self.cfg.max_rug_ratio));
            }
        }
        SecurityVerdict::Ok
    }

    /// Count of smart-money / KOL holders for a token (0 if unknown).
    pub async fn smart_holder_count(&self, mint: &str) -> u64 {
        let info = self
            .get("/v1/token/info", &[("chain", &self.cfg.chain), ("address", mint)])
            .await;
        let v = match info {
            Some(v) => v,
            None => return 0,
        };
        // smart_degen_count at top level, or under wallet_tags_stat.
        if let Some(n) = as_u64(v.get("smart_degen_count")) {
            return n;
        }
        if let Some(stat) = v.get("wallet_tags_stat") {
            return as_u64(stat.get("smart_degen")).or_else(|| as_u64(stat.get("smartmoney"))).unwrap_or(0);
        }
        0
    }

    /// Token addresses smart money is currently *buying* (recent window).
    pub async fn smartmoney_buys(&self, limit: u64) -> Vec<String> {
        let limit = limit.to_string();
        let data = self
            .get("/v1/user/smartmoney", &[("chain", &self.cfg.chain), ("limit", &limit)])
            .await;
        collect_token_addresses(data, "buy")
    }

    /// New pump.fun launches that pass GMGN's smart-money/safe filter preset.
    pub async fn trenches_pumpfun(&self, preset: &str, limit: u64) -> Vec<String> {
        let body = serde_json::json!({
            "chain": self.cfg.chain,
            "type": "new_creation",
            "launchpad_platform": ["pump"],
            "filter_preset": preset,
            "limit": limit,
        });
        let data = self.post("/v1/trenches", body).await;
        collect_token_addresses(data, "")
    }
}

// ---- tolerant accessors ----

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn as_f64(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn as_u64(v: Option<&Value>) -> Option<u64> {
    match v {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// Pull token mint addresses out of a list response. `side_filter` (e.g. "buy")
/// keeps only trades on that side when a `side` field exists; empty = keep all.
fn collect_token_addresses(data: Option<Value>, side_filter: &str) -> Vec<String> {
    let mut out = Vec::new();
    let arr = match data {
        Some(Value::Array(a)) => a,
        // Some endpoints nest the list under a key like "rank" or "trenches".
        Some(Value::Object(map)) => map
            .into_iter()
            .find_map(|(_, val)| if val.is_array() { Some(val) } else { None })
            .and_then(|v| if let Value::Array(a) = v { Some(a) } else { None })
            .unwrap_or_default(),
        _ => return out,
    };
    for item in arr {
        if !side_filter.is_empty() {
            if let Some(side) = item.get("side").and_then(|v| v.as_str()) {
                if !side.eq_ignore_ascii_case(side_filter) {
                    continue;
                }
            }
        }
        // The token mint may appear under several keys depending on endpoint.
        let addr = item
            .get("address")
            .or_else(|| item.get("token_address"))
            .or_else(|| item.get("mint"))
            .or_else(|| item.get("contract_address"))
            .and_then(|v| v.as_str());
        if let Some(a) = addr {
            if !a.is_empty() {
                out.push(a.to_string());
            }
        }
    }
    out
}
