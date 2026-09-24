//! Caches OpenWeather and ecobee data and serves it over HTTP.

mod ecobee;
mod metrics;
mod weather;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use metrics::Metrics;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::MissedTickBehavior;
use tracing::{error, info};

const WEATHER_INTERVAL: Duration = Duration::from_secs(5 * 60);
const ECOBEE_INTERVAL: Duration = Duration::from_secs(3 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Config {
    pub lat: String,
    pub lon: String,
    pub app_id: String,
    pub units: String,
    /// When unset, ecobee polling is disabled.
    pub ecobee_api_key: Option<String>,
    pub token_file: PathBuf,
    pub bind_addr: SocketAddr,
    pub owm_base_url: String,
    pub ecobee_base_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Builds a config from `lookup`, treating empty values as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |k: &str| lookup(k).filter(|v| !v.is_empty());
        let required = |k: &str| get(k).with_context(|| format!("{k} must be set"));
        Ok(Self {
            lat: required("LAT")?,
            lon: required("LON")?,
            app_id: required("APP_ID")?,
            units: get("UNITS").unwrap_or_else(|| "imperial".into()),
            ecobee_api_key: get("ECOBEE_API_KEY"),
            token_file: get("ECOBEE_TOKEN_FILE")
                .unwrap_or_else(|| "ecobee_tokens.json".into())
                .into(),
            bind_addr: get("BIND_ADDR")
                .as_deref()
                .unwrap_or("0.0.0.0:5000")
                .parse()
                .context("invalid BIND_ADDR")?,
            owm_base_url: "https://api.openweathermap.org".into(),
            ecobee_base_url: "https://api.ecobee.com".into(),
        })
    }
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    config: Config,
    http: reqwest::Client,
    /// Raw OpenWeather One Call response body.
    weather: RwLock<String>,
    ecobee_home: Arc<RwLock<Option<ecobee::EcobeeHome>>>,
    metrics: Metrics,
    /// Serializes access to the token file, so a refresh can't race an authorization.
    tokens: Mutex<()>,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!("weather_cache/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let ecobee_home = Arc::new(RwLock::new(None));
        Ok(Self(Arc::new(Inner {
            config,
            http,
            weather: RwLock::new("{}".into()),
            metrics: Metrics::new(ecobee::Readings(ecobee_home.clone())),
            ecobee_home,
            tokens: Mutex::new(()),
        })))
    }

    /// Starts the background tasks that refresh the caches. Each runs immediately, then on its interval.
    pub fn spawn_pollers(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut ticker = ticker(WEATHER_INTERVAL);
            loop {
                ticker.tick().await;
                if let Err(e) = state.update_weather().await {
                    error!("error updating weather: {e:#}");
                }
            }
        });

        if self.0.config.ecobee_api_key.is_none() {
            info!("ECOBEE_API_KEY not set; ecobee polling disabled");
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let mut ticker = ticker(ECOBEE_INTERVAL);
            loop {
                ticker.tick().await;
                if let Err(e) = state.update_ecobee_home().await {
                    error!("error updating ecobee home: {e:#}");
                }
            }
        });
    }
}

fn ticker(period: Duration) -> tokio::time::Interval {
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(show_weather))
        .route("/owm_oneshot", get(show_weather))
        .route("/ecobee_authorize", get(ecobee::authorize))
        .route("/ecobee_complete_auth", get(ecobee::complete_auth))
        .route("/ecobee_home", get(show_ecobee_home))
        .route("/epoch", get(show_epoch))
        .route("/metrics", get(show_metrics))
        .with_state(state)
}

async fn show_weather(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.0.weather.read().unwrap().clone();
    ([(header::CONTENT_TYPE, "application/json")], body)
}

async fn show_ecobee_home(State(state): State<AppState>) -> Response {
    match state.0.ecobee_home.read().unwrap().clone() {
        Some(home) => Json(home).into_response(),
        None => Json(json!({})).into_response(),
    }
}

async fn show_epoch() -> Json<u64> {
    Json(now().as_secs())
}

async fn show_metrics(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let mut body = String::new();
    prometheus_client::encoding::text::encode(&mut body, &state.0.metrics.registry)?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        body,
    ))
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the unix epoch")
}

impl AppState {
    /// Sends `req` and reads the body as text, recording the request under `endpoint`.
    ///
    /// reqwest errors include the request URL, which carries API keys and refresh tokens in its
    /// query string, so the URL is stripped before the error can reach a log line or response.
    async fn fetch(
        &self,
        endpoint: &'static str,
        req: reqwest::RequestBuilder,
    ) -> Result<(StatusCode, String)> {
        let start = Instant::now();
        let result = async {
            let resp = req.send().await.map_err(reqwest::Error::without_url)?;
            let status = resp.status();
            let body = resp.text().await.map_err(reqwest::Error::without_url)?;
            Ok((status, body))
        }
        .await;
        let status = result.as_ref().ok().map(|(status, _)| *status);
        self.0
            .metrics
            .record_upstream(endpoint, status, start.elapsed());
        result
    }
}

/// An error returned from an HTTP handler as `{"error": message}`.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e.into()))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
