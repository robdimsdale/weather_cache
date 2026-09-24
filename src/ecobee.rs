//! ecobee PIN authorization, token storage and thermostat polling.

use std::path::Path;

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::{ApiError, AppState, fetch, now};

/// Contents of the token file.
///
/// Holds either a `pending_code` between `/ecobee_authorize` and `/ecobee_complete_auth`, or the
/// token response from ecobee plus the computed `expires_at`. Unknown fields are kept so the file
/// round-trips unchanged.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Tokens {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// Unix time in seconds, a minute before the access token actually expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<f64>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl Tokens {
    /// Parses a successful response from ecobee's `/token` endpoint.
    fn from_token_response(body: &str) -> Result<Self> {
        let mut tokens: Tokens =
            serde_json::from_str(body).context("parsing ecobee token response")?;
        let expires_in = tokens
            .extra
            .get("expires_in")
            .and_then(Value::as_f64)
            .context("ecobee token response is missing expires_in")?;
        if tokens.access_token.is_none() {
            bail!("ecobee token response is missing access_token");
        }
        tokens.expires_at = Some(now().as_secs_f64() + expires_in - 60.0);
        Ok(tokens)
    }
}

async fn load_tokens(path: &Path) -> Result<Tokens> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tokens::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Atomically replaces the token file, readable only by the owner.
async fn save_tokens(path: &Path, tokens: &Tokens) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts
        .open(&tmp)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec(tokens)?).await?;
    file.sync_all().await?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("writing {}", path.display()))
}

impl AppState {
    fn ecobee_api_key(&self) -> Result<&str> {
        self.0
            .config
            .ecobee_api_key
            .as_deref()
            .context("ECOBEE_API_KEY is not set")
    }

    fn ecobee_url(&self, path: &str) -> String {
        format!("{}{path}", self.0.config.ecobee_base_url)
    }

    /// Returns a valid access token, refreshing it if expired, or `None` if not yet authorized.
    async fn ecobee_access_token(&self) -> Result<Option<String>> {
        let path = &self.0.config.token_file;
        let _guard = self.0.tokens.lock().await;
        let tokens = load_tokens(path).await?;
        let Some(access_token) = tokens.access_token else {
            return Ok(None);
        };
        if now().as_secs_f64() < tokens.expires_at.unwrap_or(0.0) {
            return Ok(Some(access_token));
        }

        info!("refreshing ecobee access token");
        let refresh_token = tokens
            .refresh_token
            .context("ecobee access token expired and no refresh token is stored")?;
        let req = self.0.http.post(self.ecobee_url("/token")).query(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
            ("client_id", self.ecobee_api_key()?),
        ]);
        let (status, body) = fetch(req).await?;
        if !status.is_success() {
            bail!("error refreshing ecobee token: {status}: {body}");
        }
        let tokens = Tokens::from_token_response(&body)?;
        save_tokens(path, &tokens).await?;
        Ok(tokens.access_token)
    }

    /// Fetches the first registered thermostat and caches a summary of it.
    /// On failure the previously cached summary is kept.
    pub async fn update_ecobee_home(&self) -> Result<()> {
        info!("updating ecobee home");
        let Some(access_token) = self.ecobee_access_token().await? else {
            warn!("no ecobee access token - call /ecobee_authorize to set up authentication");
            return Ok(());
        };
        let selection = json!({
            "selection": {
                "selectionType": "registered",
                "selectionMatch": "",
                "includeRuntime": true,
                "includeSettings": true,
                "includeWeather": true,
                "includeSensors": true,
            }
        });
        let req = self
            .0
            .http
            .get(self.ecobee_url("/1/thermostat"))
            .bearer_auth(access_token)
            .query(&[("json", selection.to_string())]);
        let (status, body) = fetch(req).await?;
        if !status.is_success() {
            bail!("ecobee returned {status}: {body}");
        }
        let resp: ThermostatResponse =
            serde_json::from_str(&body).context("parsing ecobee thermostat response")?;
        let thermostat = resp
            .thermostat_list
            .into_iter()
            .next()
            .context("ecobee returned no thermostats")?;
        *self.0.ecobee_home.write().unwrap() = Some(EcobeeHome::from_thermostat(thermostat)?);
        info!("ecobee home updated");
        Ok(())
    }
}

#[derive(Deserialize)]
struct AuthorizeResponse {
    #[serde(rename = "ecobeePin")]
    ecobee_pin: String,
    code: String,
    expires_in: u64,
}

/// Starts ecobee PIN authorization. Replaces any stored tokens with the pending code.
pub(crate) async fn authorize(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let api_key = state
        .ecobee_api_key()
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let req = state.0.http.get(state.ecobee_url("/authorize")).query(&[
        ("response_type", "ecobeePin"),
        ("client_id", api_key),
        ("scope", "smartRead"),
    ]);
    let (status, body) = fetch(req).await?;
    if !status.is_success() {
        return Err(ApiError::new(status, body));
    }
    let data: AuthorizeResponse =
        serde_json::from_str(&body).context("parsing ecobee authorize response")?;

    let _guard = state.0.tokens.lock().await;
    let pending = Tokens {
        pending_code: Some(data.code),
        ..Default::default()
    };
    save_tokens(&state.0.config.token_file, &pending).await?;
    Ok(Json(json!({
        "pin": data.ecobee_pin,
        "expires_in_seconds": data.expires_in,
        "instructions": "Go to ecobee.com > My Apps and enter this PIN, then call /ecobee_complete_auth",
    })))
}

/// Exchanges the pending code from `/ecobee_authorize` for access and refresh tokens.
pub(crate) async fn complete_auth(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let api_key = state
        .ecobee_api_key()
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    let path = &state.0.config.token_file;
    let _guard = state.0.tokens.lock().await;
    let Some(code) = load_tokens(path).await?.pending_code else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "No pending authorization. Call /ecobee_authorize first.",
        ));
    };
    let req = state.0.http.post(state.ecobee_url("/token")).query(&[
        ("grant_type", "ecobeePin"),
        ("code", &code),
        ("client_id", api_key),
    ]);
    let (status, body) = fetch(req).await?;
    if !status.is_success() {
        return Err(ApiError::new(status, body));
    }
    save_tokens(path, &Tokens::from_token_response(&body)?).await?;
    Ok(Json(json!({ "status": "authorized" })))
}

// The subset of ecobee's thermostat response that we read.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThermostatResponse {
    thermostat_list: Vec<Thermostat>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thermostat {
    name: String,
    runtime: Runtime,
    settings: Settings,
    #[serde(default)]
    equipment_status: String,
    weather: Weather,
    #[serde(default)]
    remote_sensors: Vec<RemoteSensor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Runtime {
    connected: bool,
    actual_temperature: i64,
    raw_temperature: i64,
    actual_humidity: i64,
    desired_heat: i64,
    desired_cool: i64,
    desired_fan_mode: String,
    desired_humidity: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    hvac_mode: String,
}

#[derive(Deserialize)]
struct Weather {
    forecasts: Vec<Forecast>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Forecast {
    temperature: i64,
    relative_humidity: i64,
    condition: String,
    dewpoint: i64,
    wind_speed: i64,
    wind_direction: String,
}

#[derive(Deserialize)]
struct RemoteSensor {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    capability: Vec<Capability>,
}

#[derive(Deserialize)]
struct Capability {
    #[serde(rename = "type")]
    kind: String,
    value: String,
}

// The summary served from /ecobee_home. Temperatures are converted from ecobee's tenths of a degree.

#[derive(Debug, Clone, Serialize)]
pub struct EcobeeHome {
    name: String,
    connected: bool,
    hvac_mode: String,
    equipment_status: String,
    indoor: Indoor,
    outdoor: Outdoor,
    setpoints: Setpoints,
    sensors: Vec<Sensor>,
}

#[derive(Debug, Clone, Serialize)]
struct Indoor {
    temperature: f64,
    raw_temperature: f64,
    humidity: i64,
}

#[derive(Debug, Clone, Serialize)]
struct Outdoor {
    temperature: f64,
    humidity: i64,
    condition: String,
    dewpoint: f64,
    wind_speed: i64,
    wind_direction: String,
}

#[derive(Debug, Clone, Serialize)]
struct Setpoints {
    heat: f64,
    cool: f64,
    fan_mode: String,
    humidity: i64,
}

#[derive(Debug, Clone, Serialize)]
struct Sensor {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    occupancy: Option<bool>,
}

fn tenths(v: i64) -> f64 {
    v as f64 / 10.0
}

impl EcobeeHome {
    fn from_thermostat(t: Thermostat) -> Result<Self> {
        let forecast = t
            .weather
            .forecasts
            .into_iter()
            .next()
            .context("ecobee returned no weather forecasts")?;
        let runtime = t.runtime;
        let sensors = t
            .remote_sensors
            .into_iter()
            .map(|s| {
                let mut sensor = Sensor {
                    name: s.name,
                    kind: s.kind,
                    temperature: None,
                    occupancy: None,
                };
                for cap in s.capability {
                    match cap.kind.as_str() {
                        // "unknown" (or anything else unparseable) leaves the temperature out.
                        "temperature" => sensor.temperature = cap.value.parse().ok().map(tenths),
                        "occupancy" => sensor.occupancy = Some(cap.value == "true"),
                        _ => {}
                    }
                }
                sensor
            })
            .collect();

        Ok(Self {
            name: t.name,
            connected: runtime.connected,
            hvac_mode: t.settings.hvac_mode,
            equipment_status: t.equipment_status,
            indoor: Indoor {
                temperature: tenths(runtime.actual_temperature),
                raw_temperature: tenths(runtime.raw_temperature),
                humidity: runtime.actual_humidity,
            },
            outdoor: Outdoor {
                temperature: tenths(forecast.temperature),
                humidity: forecast.relative_humidity,
                condition: forecast.condition,
                dewpoint: tenths(forecast.dewpoint),
                wind_speed: forecast.wind_speed,
                wind_direction: forecast.wind_direction,
            },
            setpoints: Setpoints {
                heat: tenths(runtime.desired_heat),
                cool: tenths(runtime.desired_cool),
                fan_mode: runtime.desired_fan_mode,
                humidity: runtime.desired_humidity,
            },
            sensors,
        })
    }
}
