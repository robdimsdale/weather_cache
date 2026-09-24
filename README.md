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
