# weather_cache
A small service that caches [OpenWeather One Call](https://openweathermap.org/api/one-call-3) and ecobee thermostat data, and serves it over HTTP.

Weather is refreshed every 5 minutes and ecobee data every 3 minutes. If a refresh fails, the last good response keeps being served.

### Endpoints

| Path | Description |
| --- | --- |
| `/`, `/owm_oneshot` | Cached OpenWeather One Call response, verbatim (`{}` until the first fetch) |
| `/ecobee_home` | Summary of the first registered ecobee thermostat (`{}` until the first fetch) |
| `/ecobee_authorize` | Start ecobee PIN authorization |
| `/ecobee_complete_auth` | Finish ecobee authorization after entering the PIN at ecobee.com > My Apps |
| `/epoch` | Current Unix time in seconds |
| `/metrics` | Prometheus/OpenMetrics metrics (see below) |

### Metrics

`/metrics` serves metrics in the OpenMetrics text format, all prefixed with `weather_cache_`.

Refreshes keep serving the last good data when they fail, so the main thing to alert on is staleness, e.g. `time() - weather_cache_last_refresh_success_timestamp_seconds > 900`.

| Metric | Labels | Description |
| --- | --- | --- |
| `refreshes_total` | `source`, `result` | Cache refreshes. `source` is `weather` or `ecobee`; `result` is `success`, `error` or `unauthorized` (ecobee only, when no tokens are stored) |
| `last_refresh_attempt_timestamp_seconds` | `source` | Unix time of the last refresh attempt |
| `last_refresh_success_timestamp_seconds` | `source` | Unix time of the last successful refresh. Absent until the first success |
| `upstream_requests_total` | `endpoint`, `status` | Requests to OpenWeather and ecobee. `status` is the HTTP status, or `error` if no response was received |
| `upstream_request_duration_seconds` | `endpoint` | Histogram of upstream request durations |
| `ecobee_authorized` | | 1 if ecobee tokens are stored. Also 0 when ecobee polling is disabled |
| `ecobee_token_refreshes_total` | `result` | ecobee access token refreshes |
| `build_info` | `version` | Always 1 |

Once ecobee data has been fetched, the thermostat readings are exported too: `ecobee_connected`, `ecobee_{indoor,outdoor}_temperature_fahrenheit`, `ecobee_{indoor,outdoor}_humidity_percent`, `ecobee_setpoint_{heat,cool}_fahrenheit`, `ecobee_setpoint_humidity_percent`, `ecobee_hvac_mode{mode}`, `ecobee_equipment_running{equipment}`, `ecobee_sensor_temperature_fahrenheit{sensor}` and `ecobee_sensor_occupied{sensor}`.

### Configuration

Configuration comes from environment variables. See [env.example](env.example).

| Variable | Required | Default |
| --- | --- | --- |
| `LAT`, `LON` | yes | |
| `APP_ID` | yes | OpenWeather API key |
| `UNITS` | no | `imperial` |
| `ECOBEE_API_KEY` | no | unset disables ecobee polling |
| `ECOBEE_TOKEN_FILE` | no | `ecobee_tokens.json` (relative to the working directory) |
| `BIND_ADDR` | no | `0.0.0.0:5000` |
| `RUST_LOG` | no | `info` |

### Running locally

```
LAT=<some-latitude> \
LON=<some-longitude> \
APP_ID=<API key from open weather> \
cargo run
```

Run the tests with `cargo test`.
