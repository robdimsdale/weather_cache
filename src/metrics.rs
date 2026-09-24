//! Prometheus metrics served from `/metrics`.

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::http::StatusCode;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::{Registry, Unit};

use crate::now;

pub(crate) const WEATHER: &str = "weather";
pub(crate) const ECOBEE: &str = "ecobee";

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceLabels {
    source: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RefreshLabels {
    source: &'static str,
    result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct EndpointLabels {
    endpoint: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct UpstreamLabels {
    endpoint: &'static str,
    /// The HTTP status code, or "error" if no response was received.
    status: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabels {
    result: &'static str,
}

type Timestamp = Gauge<f64, AtomicU64>;

pub(crate) struct Metrics {
    pub registry: Registry,
    refreshes: Family<RefreshLabels, Counter>,
    last_attempt: Family<SourceLabels, Timestamp>,
    last_success: Family<SourceLabels, Timestamp>,
    upstream_requests: Family<UpstreamLabels, Counter>,
    upstream_duration: Family<EndpointLabels, Histogram, fn() -> Histogram>,
    pub ecobee_authorized: Gauge,
    token_refreshes: Family<ResultLabels, Counter>,
}

impl Metrics {
    /// `readings` is scraped alongside the metrics recorded here.
    pub fn new(readings: impl Collector) -> Self {
        let mut metrics = Self {
            registry: Registry::with_prefix("weather_cache"),
            refreshes: Family::default(),
            last_attempt: Family::default(),
            last_success: Family::default(),
            upstream_requests: Family::default(),
            // 50ms to ~25s, just under the HTTP timeout.
            upstream_duration: Family::new_with_constructor(|| {
                Histogram::new(exponential_buckets(0.05, 2.0, 10))
            }),
            ecobee_authorized: Gauge::default(),
            token_refreshes: Family::default(),
        };
        let registry = &mut metrics.registry;

        registry.register(
            "build",
            "Build information",
            Info::new(vec![("version", env!("CARGO_PKG_VERSION"))]),
        );
        registry.register(
            "refreshes",
            "Cache refreshes by source and result (success, error or unauthorized)",
            metrics.refreshes.clone(),
        );
        registry.register_with_unit(
            "last_refresh_attempt_timestamp",
            "Unix time of the last cache refresh attempt",
            Unit::Seconds,
            metrics.last_attempt.clone(),
        );
        registry.register_with_unit(
            "last_refresh_success_timestamp",
            "Unix time of the last successful cache refresh",
            Unit::Seconds,
            metrics.last_success.clone(),
        );
        registry.register(
            "upstream_requests",
            "Requests to OpenWeather and ecobee by endpoint and HTTP status",
            metrics.upstream_requests.clone(),
        );
        registry.register_with_unit(
            "upstream_request_duration",
            "Duration of requests to OpenWeather and ecobee",
            Unit::Seconds,
            metrics.upstream_duration.clone(),
        );
        registry.register(
            "ecobee_authorized",
            "Whether ecobee access tokens are stored",
            metrics.ecobee_authorized.clone(),
        );
        registry.register(
            "ecobee_token_refreshes",
            "ecobee access token refreshes by result",
            metrics.token_refreshes.clone(),
        );
        registry.register_collector(Box::new(readings));
        metrics
    }

    /// Records a cache refresh. `result` is "success", "error" or "unauthorized".
    pub fn record_refresh(&self, source: &'static str, result: &'static str) {
        let now = now().as_secs_f64();
        let labels = SourceLabels { source };
        self.last_attempt.get_or_create(&labels).set(now);
        if result == "success" {
            self.last_success.get_or_create(&labels).set(now);
        }
        self.refreshes
            .get_or_create(&RefreshLabels { source, result })
            .inc();
    }

    /// Records a request to an upstream API. `status` is `None` if no response was received.
    pub fn record_upstream(
        &self,
        endpoint: &'static str,
        status: Option<StatusCode>,
        elapsed: Duration,
    ) {
        let status = status.map_or("error".into(), |s| s.as_u16().to_string());
        self.upstream_requests
            .get_or_create(&UpstreamLabels { endpoint, status })
            .inc();
        self.upstream_duration
            .get_or_create(&EndpointLabels { endpoint })
            .observe(elapsed.as_secs_f64());
    }

    pub fn record_token_refresh(&self, success: bool) {
        let result = if success { "success" } else { "error" };
        self.token_refreshes
            .get_or_create(&ResultLabels { result })
            .inc();
    }
}
