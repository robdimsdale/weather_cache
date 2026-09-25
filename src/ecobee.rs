//! ecobee PIN authorization, token storage and thermostat polling.

use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeGaugeValue};
use prometheus_client::metrics::MetricType;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::metrics::ECOBEE;
use crate::{ApiError, AppState, now};

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
        self.0
            .metrics
            .ecobee_authorized
            .set(tokens.access_token.is_some().into());
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
        let result = self.refresh_ecobee_tokens(path, &refresh_token).await;
        self.0.metrics.record_token_refresh(result.is_ok());
        Ok(result?.access_token)
    }

    /// Exchanges `refresh_token` for new tokens and saves them. The caller must hold the tokens lock.
    async fn refresh_ecobee_tokens(&self, path: &Path, refresh_token: &str) -> Result<Tokens> {
        let req = self.0.http.post(self.ecobee_url("/token")).query(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.ecobee_api_key()?),
        ]);
        let (status, body) = self.fetch("token", req).await?;
        if !status.is_success() {
            bail!("error refreshing ecobee token: {status}: {body}");
        }
        let tokens = Tokens::from_token_response(&body)?;
        save_tokens(path, &tokens).await?;
        Ok(tokens)
    }

    /// Fetches the first registered thermostat and caches a summary of it.
    /// On failure the previously cached summary is kept.
    pub async fn update_ecobee_home(&self) -> Result<()> {
        let result = self.fetch_ecobee_home().await;
        let outcome = match result {
            Ok(true) => "success",
            Ok(false) => "unauthorized",
            Err(_) => "error",
        };
        self.0.metrics.record_refresh(ECOBEE, outcome);
        result.map(|_| ())
    }

    /// Returns false, without fetching, if ecobee is not yet authorized.
    async fn fetch_ecobee_home(&self) -> Result<bool> {
        info!("updating ecobee home");
        let Some(access_token) = self.ecobee_access_token().await? else {
            warn!("no ecobee access token - call /ecobee_authorize to set up authentication");
            return Ok(false);
        };
        let selection = json!({
            "selection": {
                "selectionType": "registered",
                "selectionMatch": "",
                "includeRuntime": true,
                "includeSettings": true,
                "includeWeather": true,
                "includeSensors": true,
                "includeEquipmentStatus": true,
                "includeProgram": true,
                "includeEvents": true,
                "includeExtendedRuntime": true,
            }
        });
        let req = self
            .0
            .http
            .get(self.ecobee_url("/1/thermostat"))
            .bearer_auth(access_token)
            .query(&[("json", selection.to_string())]);
        let (status, body) = self.fetch("thermostat", req).await?;
        if !status.is_success() {
            bail!("ecobee returned {status}: {body}");
        }
        let resp: ThermostatResponse =
            serde_json::from_str(&body).context("parsing ecobee thermostat response")?;
        let mut thermostat = resp
            .thermostat_list
            .into_iter()
            .next()
            .context("ecobee returned no thermostats")?;
        let runtime = std::mem::take(&mut thermostat.extended_runtime);
        let first_interval = runtime.first_interval()?;
        let home = EcobeeHome::from_thermostat(thermostat)?;
        self.0
            .metrics
            .record_equipment_runtime(first_interval, &runtime.equipment());
        *self.0.ecobee_home.write().unwrap() = Some(home);
        info!("ecobee home updated");
        Ok(true)
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
    let (status, body) = state.fetch("authorize", req).await?;
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
    state.0.metrics.ecobee_authorized.set(0);
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
    let (status, body) = state.fetch("token", req).await?;
    if !status.is_success() {
        return Err(ApiError::new(status, body));
    }
    save_tokens(path, &Tokens::from_token_response(&body)?).await?;
    state.0.metrics.ecobee_authorized.set(1);
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
    program: Program,
    #[serde(default)]
    events: Vec<Event>,
    extended_runtime: ExtendedRuntime,
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
    #[serde(default)]
    desired_dehumidity: i64,
}

/// Seconds each piece of equipment ran in each of the last three 5-minute intervals. The
/// thermostat uploads these every 15 minutes.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtendedRuntime {
    /// UTC date of the last interval, as YYYY-MM-DD.
    runtime_date: String,
    /// The last interval of the day (0-287); the three readings end with it.
    runtime_interval: i64,
    #[serde(default)]
    heat_pump1: Vec<u64>,
    #[serde(default)]
    heat_pump2: Vec<u64>,
    #[serde(default)]
    aux_heat1: Vec<u64>,
    #[serde(default)]
    aux_heat2: Vec<u64>,
    #[serde(default)]
    aux_heat3: Vec<u64>,
    #[serde(default)]
    cool1: Vec<u64>,
    #[serde(default)]
    cool2: Vec<u64>,
    #[serde(default)]
    fan: Vec<u64>,
    #[serde(default)]
    humidifier: Vec<u64>,
    #[serde(default)]
    dehumidifier: Vec<u64>,
    #[serde(default)]
    economizer: Vec<u64>,
    #[serde(default)]
    ventilator: Vec<u64>,
}

impl ExtendedRuntime {
    /// The first reading's interval, counted in 5-minute intervals since the Unix epoch.
    fn first_interval(&self) -> Result<i64> {
        let days = days_since_epoch(&self.runtime_date)
            .with_context(|| format!("parsing ecobee runtimeDate {:?}", self.runtime_date))?;
        Ok(days * 288 + self.runtime_interval - 2)
    }

    /// Runtimes per interval, named as in `equipmentStatus`.
    fn equipment(&self) -> [(&'static str, &[u64]); 12] {
        [
            ("heatPump", &self.heat_pump1),
            ("heatPump2", &self.heat_pump2),
            ("auxHeat1", &self.aux_heat1),
            ("auxHeat2", &self.aux_heat2),
            ("auxHeat3", &self.aux_heat3),
            ("compCool1", &self.cool1),
            ("compCool2", &self.cool2),
            ("fan", &self.fan),
            ("humidifier", &self.humidifier),
            ("dehumidifier", &self.dehumidifier),
            ("economizer", &self.economizer),
            ("ventilator", &self.ventilator),
        ]
    }
}

/// Parses a YYYY-MM-DD date into days since 1970-01-01.
fn days_since_epoch(date: &str) -> Option<i64> {
    let mut parts = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    hvac_mode: String,
    #[serde(default)]
    fan_min_on_time: i64,
    #[serde(default)]
    humidifier_mode: String,
    #[serde(default)]
    dehumidifier_mode: String,
    aux_max_outdoor_temp: Option<i64>,
    #[serde(default)]
    aux_max_outdoor_temp_enabled: bool,
    compressor_protection_min_temp: Option<i64>,
    #[serde(default)]
    heat_stages: i64,
    #[serde(default)]
    cool_stages: i64,
    #[serde(default)]
    has_heat_pump: bool,
    #[serde(default)]
    has_humidifier: bool,
    #[serde(default)]
    has_dehumidifier: bool,
    #[serde(default)]
    auto_away: bool,
    #[serde(default)]
    follow_me_comfort: bool,
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
#[serde(rename_all = "camelCase")]
struct Program {
    /// The comfort setting the schedule calls for now, ignoring holds.
    current_climate_ref: String,
    climates: Vec<Climate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Climate {
    name: String,
    climate_ref: String,
    heat_temp: i64,
    cool_temp: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    running: bool,
    /// The comfort setting a hold switches to, or empty for a temperature hold.
    #[serde(default)]
    hold_climate_ref: String,
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
    settings: SettingsSummary,
    sensors: Vec<Sensor>,
    program: ProgramSummary,
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
    dehumidity: i64,
}

#[derive(Debug, Clone, Serialize)]
struct SettingsSummary {
    /// Minimum minutes per hour the fan runs.
    fan_min_on_time: i64,
    humidifier_mode: String,
    dehumidifier_mode: String,
    /// Outdoor temperature above which auxiliary heat is locked out, if enabled.
    aux_heat_max_outdoor_temperature: Option<f64>,
    /// Outdoor temperature below which the heat pump compressor is locked out, if there is one.
    compressor_min_outdoor_temperature: Option<f64>,
    heat_stages: i64,
    cool_stages: i64,
    has_heat_pump: bool,
    has_humidifier: bool,
    has_dehumidifier: bool,
    smart_away: bool,
    follow_me: bool,
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

#[derive(Debug, Clone, Serialize)]
struct ProgramSummary {
    /// The comfort setting in effect: the running hold's, else the scheduled one. `None` during
    /// a hold that sets temperatures rather than a comfort setting.
    comfort_setting: Option<String>,
    scheduled_comfort_setting: String,
    /// The type of the running event overriding the schedule (e.g. "hold", "vacation"), if any.
    hold: Option<String>,
    comfort_settings: Vec<ComfortSetting>,
}

#[derive(Debug, Clone, Serialize)]
struct ComfortSetting {
    climate: String,
    name: String,
    heat: f64,
    cool: f64,
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
        let settings = t.settings;
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

        let running = t.events.into_iter().find(|e| e.running);
        let comfort_setting = match &running {
            Some(e) if e.hold_climate_ref.is_empty() => None,
            Some(e) => Some(e.hold_climate_ref.clone()),
            None => Some(t.program.current_climate_ref.clone()),
        };
        let program = ProgramSummary {
            comfort_setting,
            scheduled_comfort_setting: t.program.current_climate_ref,
            hold: running.map(|e| e.kind),
            comfort_settings: t
                .program
                .climates
                .into_iter()
                .map(|c| ComfortSetting {
                    climate: c.climate_ref,
                    name: c.name,
                    heat: tenths(c.heat_temp),
                    cool: tenths(c.cool_temp),
                })
                .collect(),
        };

        Ok(Self {
            name: t.name,
            connected: runtime.connected,
            hvac_mode: settings.hvac_mode,
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
                dehumidity: runtime.desired_dehumidity,
            },
            settings: SettingsSummary {
                fan_min_on_time: settings.fan_min_on_time,
                humidifier_mode: settings.humidifier_mode,
                dehumidifier_mode: settings.dehumidifier_mode,
                aux_heat_max_outdoor_temperature: settings
                    .aux_max_outdoor_temp
                    .filter(|_| settings.aux_max_outdoor_temp_enabled)
                    .map(tenths),
                compressor_min_outdoor_temperature: settings
                    .compressor_protection_min_temp
                    .filter(|_| settings.has_heat_pump)
                    .map(tenths),
                heat_stages: settings.heat_stages,
                cool_stages: settings.cool_stages,
                has_heat_pump: settings.has_heat_pump,
                has_humidifier: settings.has_humidifier,
                has_dehumidifier: settings.has_dehumidifier,
                smart_away: settings.auto_away,
                follow_me: settings.follow_me_comfort,
            },
            sensors,
            program,
        })
    }
}

// Readings from the cached summary, exported on each scrape of /metrics.

/// Values of `equipmentStatus` that are reported as 0 when not running, so they appear in
/// `/metrics` before they first run.
const EQUIPMENT: &[&str] = &[
    "heatPump",
    "heatPump2",
    "heatPump3",
    "compCool1",
    "compCool2",
    "auxHeat1",
    "auxHeat2",
    "auxHeat3",
    "fan",
    "humidifier",
    "dehumidifier",
    "ventilator",
    "economizer",
    "compHotWater",
    "auxHotWater",
];

const HVAC_MODES: &[&str] = &["auto", "auxHeatOnly", "cool", "heat", "off"];

const HUMIDIFIER_MODES: &[&str] = &["auto", "manual", "off"];

/// 1 for `current` and 0 for the other `known` values. `current` is included even if unknown.
fn one_hot<'a>(known: &[&'a str], current: &'a str) -> Vec<([&'a str; 1], i64)> {
    let mut values: Vec<_> = known
        .iter()
        .map(|&v| ([v], i64::from(v == current)))
        .collect();
    if !known.contains(&current) {
        values.push(([current], 1));
    }
    values
}

/// Exports the cached `EcobeeHome` as gauges. Nothing is exported until the first fetch.
#[derive(Debug)]
pub(crate) struct Readings(pub Arc<RwLock<Option<EcobeeHome>>>);

impl Collector for Readings {
    fn encode(&self, mut encoder: DescriptorEncoder) -> std::fmt::Result {
        let Some(home) = self.0.read().unwrap().clone() else {
            return Ok(());
        };
        let e = &mut encoder;

        gauge(
            e,
            "ecobee_connected",
            "Whether the thermostat is connected to ecobee.",
            i64::from(home.connected),
        )?;
        gauge(
            e,
            "ecobee_indoor_temperature_fahrenheit",
            "Indoor temperature.",
            home.indoor.temperature,
        )?;
        gauge(
            e,
            "ecobee_indoor_humidity_percent",
            "Indoor relative humidity.",
            home.indoor.humidity,
        )?;
        gauge(
            e,
            "ecobee_outdoor_temperature_fahrenheit",
            "Outdoor temperature from ecobee's forecast.",
            home.outdoor.temperature,
        )?;
        gauge(
            e,
            "ecobee_outdoor_humidity_percent",
            "Outdoor relative humidity from ecobee's forecast.",
            home.outdoor.humidity,
        )?;
        gauge(
            e,
            "ecobee_setpoint_heat_fahrenheit",
            "Heat setpoint.",
            home.setpoints.heat,
        )?;
        gauge(
            e,
            "ecobee_setpoint_cool_fahrenheit",
            "Cool setpoint.",
            home.setpoints.cool,
        )?;
        gauge(
            e,
            "ecobee_setpoint_humidity_percent",
            "Humidity setpoint.",
            home.setpoints.humidity,
        )?;

        gauges(
            e,
            "ecobee_hvac_mode",
            "1 for the current HVAC mode, 0 otherwise.",
            ["mode"],
            one_hot(HVAC_MODES, &home.hvac_mode),
        )?;

        let settings = &home.settings;
        gauge(
            e,
            "ecobee_setpoint_dehumidity_percent",
            "Dehumidify setpoint.",
            home.setpoints.dehumidity,
        )?;
        gauge(
            e,
            "ecobee_fan_min_on_time_minutes",
            "Minimum minutes per hour the fan runs.",
            settings.fan_min_on_time,
        )?;
        if settings.has_humidifier {
            gauges(
                e,
                "ecobee_humidifier_mode",
                "1 for the current humidifier mode, 0 otherwise.",
                ["mode"],
                one_hot(HUMIDIFIER_MODES, &settings.humidifier_mode),
            )?;
        }
        if settings.has_dehumidifier {
            gauge(
                e,
                "ecobee_dehumidifier_enabled",
                "Whether the dehumidifier is enabled.",
                i64::from(settings.dehumidifier_mode == "on"),
            )?;
        }
        if let Some(temp) = settings.aux_heat_max_outdoor_temperature {
            gauge(
                e,
                "ecobee_aux_heat_max_outdoor_temperature_fahrenheit",
                "Outdoor temperature above which auxiliary heat is locked out.",
                temp,
            )?;
        }
        if let Some(temp) = settings.compressor_min_outdoor_temperature {
            gauge(
                e,
                "ecobee_compressor_min_outdoor_temperature_fahrenheit",
                "Outdoor temperature below which the heat pump compressor is locked out.",
                temp,
            )?;
        }
        gauge(
            e,
            "ecobee_smart_away_enabled",
            "Whether Smart Home/Away is enabled.",
            i64::from(settings.smart_away),
        )?;
        gauge(
            e,
            "ecobee_follow_me_enabled",
            "Whether Follow Me is enabled.",
            i64::from(settings.follow_me),
        )?;

        let running: Vec<&str> = home
            .equipment_status
            .split(',')
            .filter(|s| !s.is_empty())
            .collect();
        let mut equipment: Vec<_> = EQUIPMENT
            .iter()
            .map(|&eq| ([eq], i64::from(running.contains(&eq))))
            .collect();
        equipment.extend(
            running
                .iter()
                .filter(|eq| !EQUIPMENT.contains(eq))
                .map(|&eq| ([eq], 1)),
        );
        gauges(
            e,
            "ecobee_equipment_running",
            "Whether each piece of HVAC equipment is running.",
            ["equipment"],
            equipment,
        )?;

        let temperatures = home
            .sensors
            .iter()
            .filter_map(|s| Some(([s.name.as_str()], s.temperature?)));
        gauges(
            e,
            "ecobee_sensor_temperature_fahrenheit",
            "Temperature at each sensor.",
            ["sensor"],
            temperatures,
        )?;
        let occupied = home
            .sensors
            .iter()
            .filter_map(|s| Some(([s.name.as_str()], i64::from(s.occupancy?))));
        gauges(
            e,
            "ecobee_sensor_occupied",
            "Whether each sensor detects occupancy.",
            ["sensor"],
            occupied,
        )?;

        let program = &home.program;
        let current = program.comfort_setting.as_deref();
        let mut comfort: Vec<_> = program
            .comfort_settings
            .iter()
            .map(|c| {
                let active = current == Some(c.climate.as_str());
                ([c.climate.as_str(), c.name.as_str()], i64::from(active))
            })
            .collect();
        if let Some(current) = current
            && !program
                .comfort_settings
                .iter()
                .any(|c| c.climate == current)
        {
            comfort.push(([current, ""], 1));
        }
        gauges(
            e,
            "ecobee_comfort_setting",
            "1 for the comfort setting in effect, 0 otherwise. All 0 during a temperature hold.",
            ["climate", "name"],
            comfort,
        )?;
        gauge(
            e,
            "ecobee_hold_active",
            "Whether a hold, vacation or other event is overriding the schedule.",
            i64::from(program.hold.is_some()),
        )?;
        gauges(
            e,
            "ecobee_comfort_setting_heat_fahrenheit",
            "Heat setpoint of each comfort setting.",
            ["climate", "name"],
            program
                .comfort_settings
                .iter()
                .map(|c| ([c.climate.as_str(), c.name.as_str()], c.heat)),
        )?;
        gauges(
            e,
            "ecobee_comfort_setting_cool_fahrenheit",
            "Cool setpoint of each comfort setting.",
            ["climate", "name"],
            program
                .comfort_settings
                .iter()
                .map(|c| ([c.climate.as_str(), c.name.as_str()], c.cool)),
        )?;
        Ok(())
    }
}

fn gauge(
    e: &mut DescriptorEncoder,
    name: &str,
    help: &str,
    value: impl EncodeGaugeValue,
) -> std::fmt::Result {
    e.encode_descriptor(name, help, None, MetricType::Gauge)?
        .encode_gauge(&value)
}

/// Encodes one gauge per `(label values, value)` pair, labelled `labels`.
fn gauges<'a, const N: usize, V: EncodeGaugeValue>(
    e: &mut DescriptorEncoder,
    name: &str,
    help: &str,
    labels: [&str; N],
    values: impl IntoIterator<Item = ([&'a str; N], V)>,
) -> std::fmt::Result {
    let mut metric = e.encode_descriptor(name, help, None, MetricType::Gauge)?;
    for (label_values, value) in values {
        let label_set: [_; N] = std::array::from_fn(|i| (labels[i], label_values[i]));
        metric.encode_family(&label_set)?.encode_gauge(&value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_since_epoch_parses_dates() {
        assert_eq!(days_since_epoch("1970-01-01"), Some(0));
        assert_eq!(days_since_epoch("1969-12-31"), Some(-1));
        assert_eq!(days_since_epoch("2000-02-29"), Some(11_016));
        assert_eq!(days_since_epoch("2026-09-25"), Some(20_721));
        assert_eq!(days_since_epoch("2026-13-01"), None);
        assert_eq!(days_since_epoch("yesterday"), None);
    }
}
