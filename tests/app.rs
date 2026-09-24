use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use weather_cache::{AppState, Config, router};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

struct Harness {
    state: AppState,
    server: MockServer,
    token_file: PathBuf,
    _dir: TempDir,
}

/// An app whose OpenWeather and ecobee base URLs both point at one mock server.
async fn harness() -> Harness {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("ecobee_tokens.json");
    let config = Config {
        lat: "37.7".into(),
        lon: "-122.4".into(),
        app_id: "testkey".into(),
        units: "metric".into(),
        ecobee_api_key: Some("myapikey".into()),
        token_file: token_file.clone(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        owm_base_url: server.uri(),
        ecobee_base_url: server.uri(),
    };
    Harness {
        state: AppState::new(config).unwrap(),
        server,
        token_file,
        _dir: dir,
    }
}

impl Harness {
    async fn get(&self, uri: &str) -> (StatusCode, String) {
        let resp = router(self.state.clone())
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn get_json(&self, uri: &str) -> (StatusCode, Value) {
        let (status, body) = self.get(uri).await;
        (status, serde_json::from_str(&body).unwrap())
    }

    fn write_tokens(&self, tokens: Value) {
        std::fs::write(&self.token_file, tokens.to_string()).unwrap();
    }

    fn read_tokens(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.token_file).unwrap()).unwrap()
    }

    fn write_valid_tokens(&self) {
        self.write_tokens(json!({
            "access_token": "mytoken",
            "refresh_token": "myrefresh",
            "expires_at": now() + 3600.0,
        }));
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let vars: Vec<(String, String)> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| {
        vars.iter()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.clone())
    }
}

mod config {
    use super::*;

    #[test]
    fn reads_env_vars() {
        let config = Config::from_lookup(lookup(&[
            ("LAT", "37.7"),
            ("LON", "-122.4"),
            ("APP_ID", "testkey"),
            ("UNITS", "metric"),
            ("ECOBEE_API_KEY", "myapikey"),
            ("ECOBEE_TOKEN_FILE", "/var/lib/weather_cache/tokens.json"),
            ("BIND_ADDR", "127.0.0.1:8080"),
        ]))
        .unwrap();
        assert_eq!(config.lat, "37.7");
        assert_eq!(config.lon, "-122.4");
        assert_eq!(config.app_id, "testkey");
        assert_eq!(config.units, "metric");
        assert_eq!(config.ecobee_api_key.as_deref(), Some("myapikey"));
        assert_eq!(
            config.token_file,
            PathBuf::from("/var/lib/weather_cache/tokens.json")
        );
        assert_eq!(config.bind_addr.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn defaults() {
        let config =
            Config::from_lookup(lookup(&[("LAT", "0"), ("LON", "0"), ("APP_ID", "key")])).unwrap();
        assert_eq!(config.units, "imperial");
        assert_eq!(config.ecobee_api_key, None);
        assert_eq!(config.token_file, PathBuf::from("ecobee_tokens.json"));
        assert_eq!(config.bind_addr.to_string(), "0.0.0.0:5000");
    }

    #[test]
    fn empty_values_are_unset() {
        let config = Config::from_lookup(lookup(&[
            ("LAT", "0"),
            ("LON", "0"),
            ("APP_ID", "key"),
            ("UNITS", ""),
            ("ECOBEE_API_KEY", ""),
        ]))
        .unwrap();
        assert_eq!(config.units, "imperial");
        assert_eq!(config.ecobee_api_key, None);
    }

    #[test]
    fn requires_weather_vars() {
        let err =
            Config::from_lookup(lookup(&[("LAT", "0"), ("LON", "0"), ("APP_ID", "")])).unwrap_err();
        assert_eq!(err.to_string(), "APP_ID must be set");
    }
}

mod epoch {
    use super::*;

    #[tokio::test]
    async fn returns_current_time() {
        let h = harness().await;
        let before = now() as u64;
        let (status, body) = h.get_json("/epoch").await;
        let after = now() as u64;
        assert_eq!(status, StatusCode::OK);
        let epoch = body.as_u64().unwrap();
        assert!(before <= epoch && epoch <= after);
    }
}

mod weather {
    use super::*;

    fn mock_onecall(status: u16, body: &str) -> Mock {
        Mock::given(method("GET"))
            .and(path("/data/3.0/onecall"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
    }

    #[tokio::test]
    async fn empty_object_before_first_fetch() {
        let h = harness().await;
        for uri in ["/", "/owm_oneshot"] {
            assert_eq!(h.get(uri).await, (StatusCode::OK, "{}".into()));
        }
    }

    #[tokio::test]
    async fn success_caches_response_verbatim() {
        let h = harness().await;
        mock_onecall(200, r#"{"weather": "sunny"}"#)
            .mount(&h.server)
            .await;
        h.state.update_weather().await.unwrap();
        for uri in ["/", "/owm_oneshot"] {
            assert_eq!(
                h.get(uri).await,
                (StatusCode::OK, r#"{"weather": "sunny"}"#.into())
            );
        }
    }

    #[tokio::test]
    async fn error_preserves_existing_cache() {
        let h = harness().await;
        mock_onecall(200, r#"{"original": "data"}"#)
            .up_to_n_times(1)
            .mount(&h.server)
            .await;
        mock_onecall(500, "oops").mount(&h.server).await;
        h.state.update_weather().await.unwrap();
        assert!(h.state.update_weather().await.is_err());
        assert_eq!(h.get("/owm_oneshot").await.1, r#"{"original": "data"}"#);
    }

    #[tokio::test]
    async fn passes_config_as_params() {
        let h = harness().await;
        Mock::given(method("GET"))
            .and(path("/data/3.0/onecall"))
            .and(query_param("lat", "37.7"))
            .and(query_param("lon", "-122.4"))
            .and(query_param("appid", "testkey"))
            .and(query_param("units", "metric"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&h.server)
            .await;
        h.state.update_weather().await.unwrap();
    }

    #[tokio::test]
    async fn error_does_not_leak_api_key() {
        // Nothing listens on the discard port, so the request fails to connect.
        let config = Config {
            owm_base_url: "http://127.0.0.1:9".into(),
            ..Config::from_lookup(lookup(&[("LAT", "0"), ("LON", "0"), ("APP_ID", "secret")]))
                .unwrap()
        };
        let err = AppState::new(config)
            .unwrap()
            .update_weather()
            .await
            .unwrap_err();
        assert!(!format!("{err:#}").contains("secret"), "{err:#}");
    }
}

fn ecobee_api_response() -> Value {
    json!({
        "thermostatList": [{
            "name": "Home",
            "runtime": {
                "connected": true,
                "actualTemperature": 722,
                "rawTemperature": 718,
                "actualHumidity": 45,
                "desiredHeat": 700,
                "desiredCool": 760,
                "desiredFanMode": "auto",
                "desiredHumidity": 40,
            },
            "settings": {"hvacMode": "heat"},
            "equipmentStatus": "fan",
            "weather": {
                "forecasts": [{
                    "temperature": 480,
                    "relativeHumidity": 60,
                    "condition": "Cloudy",
                    "dewpoint": 380,
                    "windSpeed": 10,
                    "windDirection": "NW",
                }]
            },
            "remoteSensors": [
                {
                    "name": "Living Room",
                    "type": "ecobee3_remote_sensor",
                    "capability": [
                        {"type": "temperature", "value": "715"},
                        {"type": "occupancy", "value": "true"},
                    ],
                },
                {
                    "name": "Bedroom",
                    "type": "ecobee3_remote_sensor",
                    "capability": [
                        {"type": "temperature", "value": "705"},
                        {"type": "occupancy", "value": "false"},
                    ],
                },
            ],
        }]
    })
}

fn thermostat_request() -> MockBuilder {
    Mock::given(method("GET")).and(path("/1/thermostat"))
}

fn mock_thermostat(status: u16, body: Value) -> Mock {
    thermostat_request().respond_with(ResponseTemplate::new(status).set_body_json(body))
}

mod access_token {
    use super::*;

    fn mock_refresh(status: u16) -> Mock {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(query_param("grant_type", "refresh_token"))
            .and(query_param("refresh_token", "myrefresh"))
            .and(query_param("client_id", "myapikey"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "access_token": "newtoken",
                "token_type": "Bearer",
                "refresh_token": "newrefresh",
                "expires_in": 3600,
                "scope": "smartRead",
            })))
    }

    #[tokio::test]
    async fn skips_update_when_not_authorized() {
        let h = harness().await;
        mock_thermostat(200, ecobee_api_response())
            .expect(0)
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
        assert_eq!(h.get_json("/ecobee_home").await.1, json!({}));
    }

    #[tokio::test]
    async fn skips_update_when_authorization_pending() {
        let h = harness().await;
        h.write_tokens(json!({"pending_code": "authcode123"}));
        mock_thermostat(200, ecobee_api_response())
            .expect(0)
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
    }

    #[tokio::test]
    async fn uses_stored_token_when_valid() {
        let h = harness().await;
        h.write_valid_tokens();
        thermostat_request()
            .and(header("authorization", "Bearer mytoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ecobee_api_response()))
            .expect(1)
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
    }

    #[tokio::test]
    async fn reads_token_file_written_by_python_app() {
        let h = harness().await;
        let expires_at = now() + 3600.0;
        std::fs::write(
            &h.token_file,
            format!(
                r#"{{"access_token": "mytoken", "token_type": "Bearer", "expires_in": 3599, "refresh_token": "myrefresh", "scope": "smartRead", "expires_at": {expires_at}}}"#
            ),
        )
        .unwrap();
        thermostat_request()
            .and(header("authorization", "Bearer mytoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ecobee_api_response()))
            .expect(1)
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
    }

    #[tokio::test]
    async fn refreshes_when_expired() {
        let h = harness().await;
        h.write_tokens(json!({
            "access_token": "oldtoken",
            "refresh_token": "myrefresh",
            "expires_at": now() - 1.0,
        }));
        mock_refresh(200).expect(1).mount(&h.server).await;
        thermostat_request()
            .and(header("authorization", "Bearer newtoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ecobee_api_response()))
            .expect(1)
            .mount(&h.server)
            .await;

        let before = now();
        h.state.update_ecobee_home().await.unwrap();

        let saved = h.read_tokens();
        assert_eq!(saved["access_token"], "newtoken");
        assert_eq!(saved["refresh_token"], "newrefresh");
        assert_eq!(saved["token_type"], "Bearer");
        let expires_at = saved["expires_at"].as_f64().unwrap();
        assert!(expires_at >= before + 3600.0 - 60.0 && expires_at <= now() + 3600.0 - 60.0);
    }

    #[tokio::test]
    async fn refresh_failure_keeps_tokens_and_cache() {
        let h = harness().await;
        let tokens = json!({
            "access_token": "oldtoken",
            "refresh_token": "myrefresh",
            "expires_at": 0,
        });
        h.write_tokens(tokens.clone());
        mock_refresh(500).mount(&h.server).await;
        mock_thermostat(200, ecobee_api_response())
            .expect(0)
            .mount(&h.server)
            .await;
        assert!(h.state.update_ecobee_home().await.is_err());
        assert_eq!(h.read_tokens(), tokens);
        assert_eq!(h.get_json("/ecobee_home").await.1, json!({}));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_tokens_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let h = harness().await;
        h.write_tokens(
            json!({"access_token": "old", "refresh_token": "myrefresh", "expires_at": 0}),
        );
        mock_refresh(200).mount(&h.server).await;
        mock_thermostat(200, ecobee_api_response())
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
        let mode = std::fs::metadata(&h.token_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

mod ecobee_authorize {
    use super::*;

    fn mock_authorize(status: u16) -> Mock {
        Mock::given(method("GET"))
            .and(path("/authorize"))
            .and(query_param("response_type", "ecobeePin"))
            .and(query_param("client_id", "myapikey"))
            .and(query_param("scope", "smartRead"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "ecobeePin": "ab12",
                "code": "authcode123",
                "scope": "smartRead",
                "expires_in": 900,
                "interval": 5,
            })))
    }

    #[tokio::test]
    async fn returns_pin_and_saves_pending_code() {
        let h = harness().await;
        h.write_valid_tokens();
        mock_authorize(200).expect(1).mount(&h.server).await;
        let (status, body) = h.get_json("/ecobee_authorize").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["pin"], "ab12");
        assert_eq!(body["expires_in_seconds"], 900);
        assert!(body["instructions"].is_string());
        assert_eq!(h.read_tokens(), json!({"pending_code": "authcode123"}));
    }

    #[tokio::test]
    async fn error_from_ecobee_propagates() {
        let h = harness().await;
        mock_authorize(500).mount(&h.server).await;
        let (status, body) = h.get_json("/ecobee_authorize").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body["error"].is_string());
        assert!(!h.token_file.exists());
    }
}

mod ecobee_complete_auth {
    use super::*;

    fn mock_token(status: u16) -> Mock {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(query_param("grant_type", "ecobeePin"))
            .and(query_param("code", "authcode123"))
            .and(query_param("client_id", "myapikey"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "access_token": "myaccesstoken",
                "token_type": "Bearer",
                "refresh_token": "myrefreshtoken",
                "expires_in": 3600,
                "scope": "smartRead",
            })))
    }

    #[tokio::test]
    async fn exchanges_code_for_tokens() {
        let h = harness().await;
        h.write_tokens(json!({"pending_code": "authcode123"}));
        mock_token(200).expect(1).mount(&h.server).await;
        let (status, body) = h.get_json("/ecobee_complete_auth").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"status": "authorized"}));

        let saved = h.read_tokens();
        assert_eq!(saved["access_token"], "myaccesstoken");
        assert_eq!(saved["refresh_token"], "myrefreshtoken");
        assert!(saved["expires_at"].is_f64());
        assert!(saved.get("pending_code").is_none());
    }

    #[tokio::test]
    async fn no_pending_code_returns_400() {
        let h = harness().await;
        let (status, _) = h.get_json("/ecobee_complete_auth").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn error_from_ecobee_propagates() {
        let h = harness().await;
        h.write_tokens(json!({"pending_code": "authcode123"}));
        mock_token(500).mount(&h.server).await;
        let (status, _) = h.get_json("/ecobee_complete_auth").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(h.read_tokens(), json!({"pending_code": "authcode123"}));
    }
}

mod ecobee_home {
    use super::*;

    async fn update_with(body: Value) -> Value {
        let h = harness().await;
        h.write_valid_tokens();
        mock_thermostat(200, body).mount(&h.server).await;
        h.state.update_ecobee_home().await.unwrap();
        let (status, home) = h.get_json("/ecobee_home").await;
        assert_eq!(status, StatusCode::OK);
        home
    }

    fn with_thermostat(f: impl FnOnce(&mut Value)) -> Value {
        let mut body = ecobee_api_response();
        f(&mut body["thermostatList"][0]);
        body
    }

    #[tokio::test]
    async fn empty_object_before_first_fetch() {
        let h = harness().await;
        assert_eq!(
            h.get_json("/ecobee_home").await,
            (StatusCode::OK, json!({}))
        );
    }

    #[tokio::test]
    async fn success_caches_summary() {
        let home = update_with(ecobee_api_response()).await;
        assert_eq!(
            home,
            json!({
                "name": "Home",
                "connected": true,
                "hvac_mode": "heat",
                "equipment_status": "fan",
                "indoor": {"temperature": 72.2, "raw_temperature": 71.8, "humidity": 45},
                "outdoor": {
                    "temperature": 48.0, "humidity": 60, "condition": "Cloudy",
                    "dewpoint": 38.0, "wind_speed": 10, "wind_direction": "NW",
                },
                "setpoints": {"heat": 70.0, "cool": 76.0, "fan_mode": "auto", "humidity": 40},
                "sensors": [
                    {"name": "Living Room", "type": "ecobee3_remote_sensor", "temperature": 71.5, "occupancy": true},
                    {"name": "Bedroom", "type": "ecobee3_remote_sensor", "temperature": 70.5, "occupancy": false},
                ],
            })
        );
    }

    #[tokio::test]
    async fn requests_selection() {
        let h = harness().await;
        h.write_valid_tokens();
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
        thermostat_request()
            .and(query_param("json", selection.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(ecobee_api_response()))
            .expect(1)
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
    }

    #[tokio::test]
    async fn temperatures_converted_from_tenths() {
        let home = update_with(with_thermostat(|t| {
            t["runtime"]["actualTemperature"] = json!(685);
            t["weather"]["forecasts"][0]["temperature"] = json!(325);
        }))
        .await;
        assert_eq!(home["indoor"]["temperature"], 68.5);
        assert_eq!(home["outdoor"]["temperature"], 32.5);
    }

    #[tokio::test]
    async fn unknown_sensor_temperature_omitted() {
        let home = update_with(with_thermostat(|t| {
            t["remoteSensors"] = json!([{
                "name": "Garage",
                "type": "ecobee3_remote_sensor",
                "capability": [{"type": "temperature", "value": "unknown"}],
            }]);
        }))
        .await;
        assert_eq!(
            home["sensors"],
            json!([{"name": "Garage", "type": "ecobee3_remote_sensor"}])
        );
    }

    #[tokio::test]
    async fn optional_fields_default_when_absent() {
        let home = update_with(with_thermostat(|t| {
            let t = t.as_object_mut().unwrap();
            t.remove("equipmentStatus");
            t.remove("remoteSensors");
        }))
        .await;
        assert_eq!(home["equipment_status"], "");
        assert_eq!(home["sensors"], json!([]));
    }

    #[tokio::test]
    async fn error_preserves_existing_cache() {
        let h = harness().await;
        h.write_valid_tokens();
        mock_thermostat(200, ecobee_api_response())
            .up_to_n_times(1)
            .mount(&h.server)
            .await;
        mock_thermostat(500, json!({"status": {"code": 3}}))
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
        let (_, before) = h.get_json("/ecobee_home").await;
        assert!(h.state.update_ecobee_home().await.is_err());
        assert_eq!(h.get_json("/ecobee_home").await.1, before);
    }

    #[tokio::test]
    async fn malformed_response_preserves_existing_cache() {
        let h = harness().await;
        h.write_valid_tokens();
        mock_thermostat(200, ecobee_api_response())
            .up_to_n_times(1)
            .mount(&h.server)
            .await;
        mock_thermostat(200, json!({"thermostatList": []}))
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
        let (_, before) = h.get_json("/ecobee_home").await;
        assert!(h.state.update_ecobee_home().await.is_err());
        assert_eq!(h.get_json("/ecobee_home").await.1, before);
    }
}

mod metrics {
    use super::*;

    impl Harness {
        /// Returns the value of the sample `series` (a metric name plus labels) from `/metrics`.
        async fn metric(&self, series: &str) -> Option<f64> {
            let (status, body) = self.get("/metrics").await;
            assert_eq!(status, StatusCode::OK);
            body.lines()
                .find_map(|line| line.strip_prefix(series)?.strip_prefix(' '))
                .map(|v| v.parse().unwrap())
        }
    }

    fn mock_onecall(status: u16) -> Mock {
        Mock::given(method("GET"))
            .and(path("/data/3.0/onecall"))
            .respond_with(ResponseTemplate::new(status).set_body_string("{}"))
    }

    #[tokio::test]
    async fn served_as_openmetrics() {
        let h = harness().await;
        let resp = router(h.state.clone())
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/openmetrics-text")
        );
        assert_eq!(
            h.metric(r#"weather_cache_build_info{version="0.1.0"}"#)
                .await,
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn records_successful_refresh() {
        let h = harness().await;
        mock_onecall(200).mount(&h.server).await;
        let before = now();
        h.state.update_weather().await.unwrap();

        assert_eq!(
            h.metric(r#"weather_cache_refreshes_total{source="weather",result="success"}"#)
                .await,
            Some(1.0)
        );
        let success = h
            .metric(r#"weather_cache_last_refresh_success_timestamp_seconds{source="weather"}"#)
            .await
            .unwrap();
        assert!(before <= success && success <= now());
        assert_eq!(
            h.metric(r#"weather_cache_upstream_requests_total{endpoint="onecall",status="200"}"#)
                .await,
            Some(1.0)
        );
        assert_eq!(
            h.metric(
                r#"weather_cache_upstream_request_duration_seconds_count{endpoint="onecall"}"#
            )
            .await,
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn failed_refresh_keeps_last_success() {
        let h = harness().await;
        mock_onecall(200).up_to_n_times(1).mount(&h.server).await;
        mock_onecall(500).mount(&h.server).await;
        h.state.update_weather().await.unwrap();
        let success = r#"weather_cache_last_refresh_success_timestamp_seconds{source="weather"}"#;
        let first_success = h.metric(success).await.unwrap();
        assert!(h.state.update_weather().await.is_err());

        assert_eq!(h.metric(success).await, Some(first_success));
        let attempt = h
            .metric(r#"weather_cache_last_refresh_attempt_timestamp_seconds{source="weather"}"#)
            .await
            .unwrap();
        assert!(attempt >= first_success);
        assert_eq!(
            h.metric(r#"weather_cache_refreshes_total{source="weather",result="error"}"#)
                .await,
            Some(1.0)
        );
        assert_eq!(
            h.metric(r#"weather_cache_upstream_requests_total{endpoint="onecall",status="500"}"#)
                .await,
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn unreachable_upstream_counts_as_error_status() {
        let config = Config {
            owm_base_url: "http://127.0.0.1:9".into(),
            ..Config::from_lookup(lookup(&[("LAT", "0"), ("LON", "0"), ("APP_ID", "key")])).unwrap()
        };
        let state = AppState::new(config).unwrap();
        assert!(state.update_weather().await.is_err());
        let resp = router(state)
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains(
                r#"weather_cache_upstream_requests_total{endpoint="onecall",status="error"} 1"#
            ),
            "{body}"
        );
    }

    #[tokio::test]
    async fn ecobee_unauthorized_is_not_success() {
        let h = harness().await;
        h.state.update_ecobee_home().await.unwrap();
        assert_eq!(
            h.metric(r#"weather_cache_refreshes_total{source="ecobee",result="unauthorized"}"#)
                .await,
            Some(1.0)
        );
        assert_eq!(
            h.metric(r#"weather_cache_last_refresh_success_timestamp_seconds{source="ecobee"}"#)
                .await,
            None
        );
        assert_eq!(h.metric("weather_cache_ecobee_authorized").await, Some(0.0));
    }

    #[tokio::test]
    async fn ecobee_authorized_tracks_auth_flow() {
        let h = harness().await;
        h.write_valid_tokens();
        mock_thermostat(200, ecobee_api_response())
            .mount(&h.server)
            .await;
        h.state.update_ecobee_home().await.unwrap();
        assert_eq!(h.metric("weather_cache_ecobee_authorized").await, Some(1.0));

        Mock::given(method("GET"))
            .and(path("/authorize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ecobeePin": "ab12",
                "code": "authcode123",
                "expires_in": 900,
            })))
            .mount(&h.server)
            .await;
        h.get("/ecobee_authorize").await;
        assert_eq!(h.metric("weather_cache_ecobee_authorized").await, Some(0.0));

        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "myaccesstoken",
                "refresh_token": "myrefreshtoken",
                "expires_in": 3600,
            })))
            .mount(&h.server)
            .await;
        h.get("/ecobee_complete_auth").await;
        assert_eq!(h.metric("weather_cache_ecobee_authorized").await, Some(1.0));
    }

    #[tokio::test]
    async fn records_token_refresh() {
        let h = harness().await;
        h.write_tokens(json!({
            "access_token": "oldtoken",
            "refresh_token": "myrefresh",
            "expires_at": 0,
        }));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&h.server)
            .await;
        assert!(h.state.update_ecobee_home().await.is_err());
        assert_eq!(
            h.metric(r#"weather_cache_ecobee_token_refreshes_total{result="error"}"#)
                .await,
            Some(1.0)
        );
        assert_eq!(
            h.metric(r#"weather_cache_upstream_requests_total{endpoint="token",status="400"}"#)
                .await,
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn no_ecobee_readings_before_first_fetch() {
        let h = harness().await;
        let (_, body) = h.get("/metrics").await;
        assert!(!body.contains("weather_cache_ecobee_connected"), "{body}");
    }

    #[tokio::test]
    async fn exports_ecobee_readings() {
        let h = harness().await;
        h.write_valid_tokens();
        let mut body = ecobee_api_response();
        body["thermostatList"][0]["equipmentStatus"] = json!("heatPump,fan,newThing");
        mock_thermostat(200, body).mount(&h.server).await;
        h.state.update_ecobee_home().await.unwrap();

        for (series, value) in [
            ("weather_cache_ecobee_connected", 1.0),
            ("weather_cache_ecobee_indoor_temperature_fahrenheit", 72.2),
            ("weather_cache_ecobee_indoor_humidity_percent", 45.0),
            ("weather_cache_ecobee_outdoor_temperature_fahrenheit", 48.0),
            ("weather_cache_ecobee_outdoor_humidity_percent", 60.0),
            ("weather_cache_ecobee_setpoint_heat_fahrenheit", 70.0),
            ("weather_cache_ecobee_setpoint_cool_fahrenheit", 76.0),
            ("weather_cache_ecobee_setpoint_humidity_percent", 40.0),
            (r#"weather_cache_ecobee_hvac_mode{mode="heat"}"#, 1.0),
            (r#"weather_cache_ecobee_hvac_mode{mode="cool"}"#, 0.0),
            (
                r#"weather_cache_ecobee_equipment_running{equipment="heatPump"}"#,
                1.0,
            ),
            (
                r#"weather_cache_ecobee_equipment_running{equipment="fan"}"#,
                1.0,
            ),
            (
                r#"weather_cache_ecobee_equipment_running{equipment="newThing"}"#,
                1.0,
            ),
            (
                r#"weather_cache_ecobee_equipment_running{equipment="compCool1"}"#,
                0.0,
            ),
            (
                r#"weather_cache_ecobee_sensor_temperature_fahrenheit{sensor="Living Room"}"#,
                71.5,
            ),
            (
                r#"weather_cache_ecobee_sensor_occupied{sensor="Living Room"}"#,
                1.0,
            ),
            (
                r#"weather_cache_ecobee_sensor_occupied{sensor="Bedroom"}"#,
                0.0,
            ),
        ] {
            assert_eq!(h.metric(series).await, Some(value), "{series}");
        }
    }
}
