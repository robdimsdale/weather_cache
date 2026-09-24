use anyhow::{Result, bail};
use tracing::info;

use crate::AppState;
use crate::metrics::WEATHER;

impl AppState {
    /// Fetches the OpenWeather One Call response and caches it verbatim.
    /// On failure the previously cached response is kept.
    pub async fn update_weather(&self) -> Result<()> {
        let result = self.fetch_weather().await;
        let outcome = if result.is_ok() { "success" } else { "error" };
        self.0.metrics.record_refresh(WEATHER, outcome);
        result
    }

    async fn fetch_weather(&self) -> Result<()> {
        info!("updating weather");
        let config = &self.0.config;
        let req = self
            .0
            .http
            .get(format!("{}/data/3.0/onecall", config.owm_base_url))
            .query(&[
                ("lat", &config.lat),
                ("lon", &config.lon),
                ("appid", &config.app_id),
                ("units", &config.units),
            ]);
        let (status, body) = self.fetch("onecall", req).await?;
        if !status.is_success() {
            bail!("OpenWeather returned {status}: {body}");
        }
        *self.0.weather.write().unwrap() = body;
        info!("weather updated");
        Ok(())
    }
}
