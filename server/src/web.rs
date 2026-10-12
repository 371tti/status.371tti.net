use std::{
    collections::BTreeMap,
    io,
    net::Ipv4Addr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use chrono::{DateTime, FixedOffset, Utc};
use kurosabi::{http::HttpMethod, server::tokio::KurosabiTokioServerBuilder};
use minijinja::{Environment, value::Serde};
use serde::Serialize;
use tracing::{error, warn};

mod charts;
mod dashboard;
mod detail;
use self::charts::format_milliseconds;
use self::dashboard::GroupMonitorData;

use crate::{
    config::{AppConfig, MonitorConfig, MonitorTarget, StatusMessages},
    store::{CheckRecord, Store},
};

const MAX_HISTORY: usize = 100;
const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
const NOT_FOUND_REDIRECT: &str = "https://dev.371tti.net/";
const MONITORING_UNAVAILABLE_MESSAGE: &str =
    "Monitoring system was unavailable; this interval was not measured.";

#[derive(Clone)]
pub struct WebState {
    config: AppConfig,
    store: Store,
    counters: Arc<RequestCounters>,
    started_at: Instant,
}

#[derive(Default)]
struct RequestCounters {
    total: AtomicU64,
    ping: AtomicU64,
    healthz: AtomicU64,
}

#[derive(Clone, Copy)]
enum Route<'a> {
    Dashboard,
    Ping,
    Healthz,
    Font(&'a str),
    Asset(&'a str),
    DashboardStatus,
    MonitorStatus(&'a str),
    MonitorCharts {
        id: &'a str,
        window_end: &'a str,
        start: &'a str,
        end: &'a str,
    },
    MonitorDetail(&'a str),
    NotFound,
}

impl Route<'_> {
    fn name(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Ping => "ping",
            Self::Healthz => "healthz",
            Self::Font(_) => "font",
            Self::Asset(_) => "asset",
            Self::DashboardStatus => "dashboard_status",
            Self::MonitorStatus(_) => "monitor_status",
            Self::MonitorCharts { .. } => "monitor_chart_data",
            Self::MonitorDetail(_) => "monitor_detail",
            Self::NotFound => "not_found",
        }
    }
}

fn route_for_path(path: &[String]) -> Route<'_> {
    match path {
        [root] if root.is_empty() => Route::Dashboard,
        [ping] if ping == "ping" => Route::Ping,
        [healthz] if healthz == "healthz" => Route::Healthz,
        [fonts, font]
            if fonts == "fonts"
                && matches!(
                    font.as_str(),
                    "UDEVGothicHSLG-Regular.woff2" | "UDEVGothicHSLG-Bold.woff2"
                ) =>
        {
            Route::Font(font)
        }
        [assets, asset]
            if assets == "assets"
                && matches!(
                    asset.as_str(),
                    "chart.umd.min.js"
                        | "hammer.min.js"
                        | "chartjs-plugin-zoom.min.js"
                        | "status-charts.js"
                        | "status-updates.js"
                ) =>
        {
            Route::Asset(asset)
        }
        [api, status] if api == "api" && status == "status" => Route::DashboardStatus,
        [api, monitors, id, status]
            if api == "api" && monitors == "monitors" && status == "status" =>
        {
            Route::MonitorStatus(id)
        }
        [api, monitors, id, charts, window_end, start, end]
            if api == "api" && monitors == "monitors" && charts == "charts" =>
        {
            Route::MonitorCharts {
                id,
                window_end,
                start,
                end,
            }
        }
        [monitors, id] if monitors == "monitors" => Route::MonitorDetail(id),
        _ => Route::NotFound,
    }
}

pub async fn serve(config: AppConfig, store: Store) -> std::io::Result<()> {
    let bind = config
        .server
        .bind
        .parse::<Ipv4Addr>()
        .expect("validated server.bind address");
    let port = config.server.port;
    let state = WebState {
        config,
        store,
        counters: Arc::new(RequestCounters::default()),
        started_at: Instant::now(),
    };
    let server = KurosabiTokioServerBuilder::default()
        .bind(bind.octets())
        .port(port)
        .router_and_build(move |conn| {
            let state = state.clone();
            async move {
                let total_count = state.counters.total.fetch_add(1, Ordering::Relaxed) + 1;
                let path_segments = conn.path_segs();
                let path: Vec<String> = path_segments
                    .iter()
                    .map(|segment| segment.to_string())
                    .collect();
                let route = route_for_path(&path);
                let route_name = route.name();
                if conn.req.method() != &HttpMethod::GET {
                    warn!(
                        method = ?conn.req.method(),
                        route = route_name,
                        "HTTP method not allowed"
                    );
                    return conn.set_status_code(405u16).html_body(error_page(
                        "405 Method Not Allowed",
                        "This request method is not supported.",
                    ));
                }

                // Expose only application routes; workspace and configuration files are never served.
                match route {
                    Route::Dashboard => match dashboard::dashboard(&state).await {
                        Ok(html) => conn.html_body(html),
                        Err(error) => {
                            error!(error = %error, "failed to render dashboard");
                            conn.set_status_code(500u16).html_body(error_page(
                                "500 Internal Server Error",
                                "Failed to load the monitoring dashboard.",
                            ))
                        }
                    },
                    Route::Ping => {
                        let ping_count = state.counters.ping.fetch_add(1, Ordering::Relaxed) + 1;
                        conn.text_body(format!(
                            "status.code 0\nserver.requests.unit requests\nserver.requests.total {total_count}\nserver.requests.ping {ping_count}\nserver.requests.healthz {}\nprocess.unit s\nprocess.uptime_seconds {:.1}\n",
                            state.counters.healthz.load(Ordering::Relaxed),
                            state.started_at.elapsed().as_secs_f64(),
                        ))
                    }
                    Route::Healthz => {
                        let healthz_count = state.counters.healthz.fetch_add(1, Ordering::Relaxed) + 1;
                        conn.text_body(format!(
                            "status.code 0\nserver.requests.unit requests\nserver.requests.total {total_count}\nserver.requests.ping {}\nserver.requests.healthz {healthz_count}\nprocess.unit s\nprocess.uptime_seconds {:.1}\n",
                            state.counters.ping.load(Ordering::Relaxed),
                            state.started_at.elapsed().as_secs_f64(),
                        ))
                    }
                    Route::Font("UDEVGothicHSLG-Regular.woff2") => conn
                        .add_header("Content-Type", "font/woff2")
                        .add_header("Cache-Control", "public, max-age=86400")
                        .binary_body(include_bytes!("../fonts/UDEVGothicHSLG-Regular.woff2")),
                    Route::Font("UDEVGothicHSLG-Bold.woff2") => conn
                        .add_header("Content-Type", "font/woff2")
                        .add_header("Cache-Control", "public, max-age=86400")
                        .binary_body(include_bytes!("../fonts/UDEVGothicHSLG-Bold.woff2")),
                    Route::Asset("chart.umd.min.js") => conn
                        .add_header("Content-Type", "text/javascript; charset=utf-8")
                        .add_header("Cache-Control", "public, max-age=86400")
                        .binary_body(include_bytes!("../assets/chart.umd.min.js")),
                    Route::Asset("hammer.min.js") => conn
                        .add_header("Content-Type", "text/javascript; charset=utf-8")
                        .add_header("Cache-Control", "public, max-age=86400")
                        .binary_body(include_bytes!("../assets/hammer.min.js")),
                    Route::Asset("chartjs-plugin-zoom.min.js") => {
                        conn.add_header("Content-Type", "text/javascript; charset=utf-8")
                            .add_header("Cache-Control", "public, max-age=86400")
                            .binary_body(include_bytes!("../assets/chartjs-plugin-zoom.min.js"))
                    }
                    Route::Asset("status-charts.js") => conn
                        .add_header("Content-Type", "text/javascript; charset=utf-8")
                        .add_header("Cache-Control", "public, max-age=86400")
                        .binary_body(include_bytes!("../assets/status-charts.js")),
                    Route::Asset("status-updates.js") => conn
                        .add_header("Content-Type", "text/javascript; charset=utf-8")
                        .add_header("Cache-Control", "public, max-age=86400")
                        .binary_body(include_bytes!("../assets/status-updates.js")),
                    Route::DashboardStatus => {
                        match dashboard::dashboard_data(&state).await {
                            Ok(data) => match serde_json::to_string(&data) {
                                Ok(body) => conn
                                    .add_header("Content-Type", "application/json; charset=utf-8")
                                    .add_header("Cache-Control", "no-store")
                                    .binary_body(body.as_bytes()),
                                Err(error) => {
                                    error!(error = %error, "failed to encode dashboard status");
                                    conn.set_status_code(500u16)
                                        .add_header("Content-Type", "application/json; charset=utf-8")
                                        .binary_body(b"{\"error\":\"Status data is unavailable.\"}")
                                }
                            },
                            Err(error) => {
                                error!(error = %error, "failed to load dashboard status");
                                conn.set_status_code(500u16)
                                    .add_header("Content-Type", "application/json; charset=utf-8")
                                    .binary_body(b"{\"error\":\"Status data is unavailable.\"}")
                            }
                        }
                    }
                    Route::MonitorStatus(id) => {
                        match detail::monitor_detail_data(&state, id).await {
                            Ok(Some(data)) => match serde_json::to_string(&data) {
                                Ok(body) => conn
                                    .add_header("Content-Type", "application/json; charset=utf-8")
                                    .add_header("Cache-Control", "no-store")
                                    .binary_body(body.as_bytes()),
                                Err(error) => {
                                    error!(monitor_id = %id, error = %error, "failed to encode monitor status");
                                    conn.set_status_code(500u16)
                                        .add_header("Content-Type", "application/json; charset=utf-8")
                                        .binary_body(b"{\"error\":\"Status data is unavailable.\"}")
                                }
                            },
                            Ok(None) => conn
                                .set_status_code(404u16)
                                .add_header("Content-Type", "application/json; charset=utf-8")
                                .binary_body(b"{\"error\":\"Monitor not found.\"}"),
                            Err(error) => {
                                error!(monitor_id = %id, error = %error, "failed to load monitor status");
                                conn.set_status_code(500u16)
                                    .add_header("Content-Type", "application/json; charset=utf-8")
                                    .binary_body(b"{\"error\":\"Status data is unavailable.\"}")
                            }
                        }
                    }
                    Route::MonitorCharts {
                        id,
                        window_end,
                        start,
                        end,
                    } => {
                        let parsed_range = window_end
                            .parse::<i64>()
                            .ok()
                            .zip(start.parse::<i64>().ok())
                            .zip(end.parse::<i64>().ok())
                            .map(|((window_end, start), end)| (window_end, start, end));
                        match parsed_range {
                            Some((window_end, start, end)) if start < end => {
                                match detail::monitor_chart_data(
                                    &state,
                                    id,
                                    window_end,
                                    start,
                                    end,
                                )
                                .await
                                {
                                    Ok(Some(data)) => match serde_json::to_string(&data) {
                                        Ok(body) => conn
                                            .add_header(
                                                "Content-Type",
                                                "application/json; charset=utf-8",
                                            )
                                            .add_header("Cache-Control", "no-store")
                                            .binary_body(body.as_bytes()),
                                        Err(error) => {
                                            error!(monitor_id = %id, error = %error, "failed to encode monitor chart data");
                                            conn.set_status_code(500u16)
                                                .add_header(
                                                    "Content-Type",
                                                    "application/json; charset=utf-8",
                                                )
                                                .binary_body(b"{\"error\":\"Chart data is unavailable.\"}")
                                        }
                                    },
                                    Ok(None) => conn
                                        .set_status_code(404u16)
                                        .add_header(
                                            "Content-Type",
                                            "application/json; charset=utf-8",
                                        )
                                        .binary_body(b"{\"error\":\"Monitor not found.\"}"),
                                    Err(error) if error.kind() == io::ErrorKind::InvalidInput => conn
                                        .set_status_code(400u16)
                                        .add_header(
                                            "Content-Type",
                                            "application/json; charset=utf-8",
                                        )
                                        .binary_body(b"{\"error\":\"Invalid chart range.\"}"),
                                    Err(error) => {
                                        error!(monitor_id = %id, error = %error, "failed to load monitor chart data");
                                        conn.set_status_code(500u16)
                                            .add_header(
                                                "Content-Type",
                                                "application/json; charset=utf-8",
                                            )
                                            .binary_body(b"{\"error\":\"Chart data is unavailable.\"}")
                                    }
                                }
                            }
                            _ => conn
                                .set_status_code(400u16)
                                .add_header(
                                    "Content-Type",
                                    "application/json; charset=utf-8",
                                )
                                .binary_body(b"{\"error\":\"Invalid chart range.\"}"),
                        }
                    }
                    Route::MonitorDetail(id) => {
                        match detail::monitor_detail(&state, id).await {
                            Ok(Some(html)) => conn.html_body(html),
                            Ok(None) => conn.redirect(NOT_FOUND_REDIRECT),
                            Err(error) => {
                                error!(monitor_id = %id, error = %error, "failed to render monitor detail");
                                conn.set_status_code(500u16).html_body(error_page(
                                    "500 Internal Server Error",
                                    "Failed to load service history.",
                                ))
                            }
                        }
                    }
                    Route::NotFound | Route::Font(_) | Route::Asset(_) => {
                        conn.redirect(NOT_FOUND_REDIRECT)
                    }
                }
            }
        });

    server.run().await
}

fn aggregate_status(
    configured_count: usize,
    included_count: usize,
    down_count: usize,
    slow_count: usize,
    pending_count: usize,
    messages: &StatusMessages,
) -> (&'static str, &str) {
    if configured_count == 0 {
        (
            "pending",
            status_message(&messages.no_services, "No services configured"),
        )
    } else if included_count == 0 {
        (
            "pending",
            status_message(
                &messages.no_included_services,
                "No services included in status",
            ),
        )
    } else if down_count == included_count {
        (
            "down",
            status_message(&messages.outage, "All services are down"),
        )
    } else if down_count > 0 {
        (
            "down",
            status_message(&messages.incident, "Some services are experiencing issues"),
        )
    } else if slow_count > 0 {
        (
            "slow",
            status_message(&messages.degraded, "Service performance is degraded"),
        )
    } else if pending_count > 0 {
        (
            "pending",
            status_message(&messages.checking, "Checking services"),
        )
    } else {
        (
            "up",
            status_message(&messages.operational, "All services are operational"),
        )
    }
}

fn status_message<'a>(configured: &'a Option<String>, fallback: &'static str) -> &'a str {
    configured
        .as_deref()
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or(fallback)
}

fn compact_status_label(status_class: &str) -> &'static str {
    match status_class {
        "down" => "Down",
        "slow" => "Degraded",
        "pending" => "Pending",
        _ => "Operational",
    }
}

fn is_slow_response(record: &CheckRecord, threshold_ms: Option<u64>) -> bool {
    record.is_up
        && threshold_ms.is_some_and(|threshold| {
            record
                .latency_ms
                .is_some_and(|latency| latency > threshold as f64)
                || record.metrics.iter().any(|(key, value)| {
                    key.starts_with("response.")
                        && key != "response.ms"
                        && *value > threshold as f64
                })
        })
}

fn normal_percent_for_history(history: &[CheckRecord], threshold_ms: Option<u64>) -> Option<f64> {
    if history.is_empty() {
        return None;
    }
    let normal_count = history
        .iter()
        .filter(|record| {
            record.is_up && !record.is_degraded && !is_slow_response(record, threshold_ms)
        })
        .count();
    Some(normal_count as f64 * 100.0 / history.len() as f64)
}

fn record_status(record: &CheckRecord, threshold_ms: Option<u64>) -> (&'static str, &'static str) {
    if record.is_pending {
        ("pending", "Pending")
    } else if !record.is_up {
        ("down", "Down")
    } else if record.is_degraded || is_slow_response(record, threshold_ms) {
        ("slow", "Degraded")
    } else {
        ("up", "Operational")
    }
}

#[derive(Default)]
struct PeriodCounts {
    operational: usize,
    degraded: usize,
    down: usize,
    pending: usize,
}

impl PeriodCounts {
    fn status_class(&self) -> &'static str {
        if self.down > 0 {
            "down"
        } else if self.degraded > 0 {
            "slow"
        } else if self.pending > 0 || self.operational == 0 {
            "pending"
        } else {
            "up"
        }
    }
}

fn status_bar_period(retention_start_ms: i64, index: usize, samples_per_day: u32) -> (i64, i64) {
    let per_day = samples_per_day as usize;
    let day_index = (index / per_day) as i64;
    let sample_index = (index % per_day) as i64;
    let day_start = retention_start_ms.saturating_add(day_index.saturating_mul(MILLIS_PER_DAY));
    let samples = i64::from(samples_per_day);
    (
        day_start.saturating_add(sample_index.saturating_mul(MILLIS_PER_DAY) / samples),
        day_start.saturating_add((sample_index + 1).saturating_mul(MILLIS_PER_DAY) / samples),
    )
}

fn monitor_status_buckets(
    history: &[CheckRecord],
    interval_seconds: u64,
    threshold_ms: Option<u64>,
    retention_start_ms: i64,
    retention_days: u32,
    samples_per_day: u32,
) -> Vec<PeriodCounts> {
    let total_segments = retention_days as usize * samples_per_day as usize;
    let mut scheduled_by_segment = (0..total_segments)
        .map(|_| BTreeMap::<i64, &CheckRecord>::new())
        .collect::<Vec<_>>();
    let interval_ms = interval_millis(interval_seconds);
    let retention_end_ms =
        retention_start_ms.saturating_add(i64::from(retention_days).saturating_mul(MILLIS_PER_DAY));
    for record in history.iter().filter(|record| {
        record.checked_at_ms >= retention_start_ms && record.checked_at_ms < retention_end_ms
    }) {
        let elapsed = record.checked_at_ms.saturating_sub(retention_start_ms);
        let day_index = elapsed / MILLIS_PER_DAY;
        let within_day = elapsed % MILLIS_PER_DAY;
        let sample_index = within_day.saturating_mul(i64::from(samples_per_day)) / MILLIS_PER_DAY;
        let segment_index = (day_index as usize * samples_per_day as usize + sample_index as usize)
            .min(total_segments.saturating_sub(1));
        let slot = schedule_slot(record.checked_at_ms, interval_ms);
        let replace = scheduled_by_segment[segment_index]
            .get(&slot)
            .is_none_or(|stored| record.checked_at_ms > stored.checked_at_ms);
        if replace {
            scheduled_by_segment[segment_index].insert(slot, record);
        }
    }

    let mut previous_status = "pending";
    scheduled_by_segment
        .into_iter()
        .enumerate()
        .map(|(index, scheduled_records)| {
            let (start_ms, end_ms) = status_bar_period(retention_start_ms, index, samples_per_day);
            let first_expected = start_ms.div_euclid(interval_ms)
                + if start_ms.rem_euclid(interval_ms) != 0 {
                    1
                } else {
                    0
                };
            let last_expected = end_ms.saturating_sub(1).div_euclid(interval_ms);
            let expected = if last_expected >= first_expected {
                (last_expected - first_expected + 1) as usize
            } else {
                0
            };
            let mut counts = PeriodCounts::default();
            for record in scheduled_records.values() {
                match record_status(record, threshold_ms).0 {
                    "up" => counts.operational += 1,
                    "slow" => counts.degraded += 1,
                    "down" => counts.down += 1,
                    _ => counts.pending += 1,
                }
            }
            counts.pending += expected.saturating_sub(scheduled_records.len());
            if expected == 0 && scheduled_records.is_empty() {
                match previous_status {
                    "up" => counts.operational = 1,
                    "slow" => counts.degraded = 1,
                    "down" => counts.down = 1,
                    _ => counts.pending = 1,
                }
            }
            previous_status = counts.status_class();
            counts
        })
        .collect()
}

fn monitor_period_status(
    monitor: &GroupMonitorData,
    retention_start_ms: i64,
    retention_days: u32,
    samples_per_day: u32,
) -> Vec<PeriodCounts> {
    monitor_status_buckets(
        &monitor.history,
        monitor.interval_seconds,
        monitor.slow_response_threshold_ms,
        retention_start_ms,
        retention_days,
        samples_per_day,
    )
}

fn monitor_uptime_bar(
    history: &[CheckRecord],
    interval_seconds: u64,
    threshold_ms: Option<u64>,
    retention_days: u32,
    samples_per_day: u32,
    now_ms: i64,
) -> (Vec<UptimeSegment>, String, String, String) {
    let retention_start_ms = now_ms.saturating_sub(i64::from(retention_days) * MILLIS_PER_DAY);
    let status_buckets = monitor_status_buckets(
        history,
        interval_seconds,
        threshold_ms,
        retention_start_ms,
        retention_days,
        samples_per_day,
    );
    let mut totals = PeriodCounts::default();
    let mut segments = Vec::with_capacity(status_buckets.len());
    for (index, counts) in status_buckets.into_iter().enumerate() {
        let (start_ms, end_ms) = status_bar_period(retention_start_ms, index, samples_per_day);
        let status_class = counts.status_class();
        match status_class {
            "up" => totals.operational += 1,
            "slow" => totals.degraded += 1,
            "down" => totals.down += 1,
            _ => totals.pending += 1,
        }
        segments.push(UptimeSegment {
            status_class: status_class.to_owned(),
            title: format!(
                "{} – {} · {} · Operational: {} · Degraded: {} · Down: {} · Pending: {}",
                format_timestamp(start_ms),
                format_timestamp(end_ms),
                compact_status_label(status_class),
                counts.operational,
                counts.degraded,
                counts.down,
                counts.pending
            ),
        });
    }
    let start_label = segments
        .first()
        .map(|_| format_duration_ago(u64::from(retention_days).saturating_mul(86_400)))
        .unwrap_or_default();
    let end_label = if segments.is_empty() {
        String::new()
    } else {
        "Now".to_owned()
    };
    let label = format!(
        "Last {retention_days} days: {} samples, {} operational, {} degraded, {} down, {} pending",
        segments.len(),
        totals.operational,
        totals.degraded,
        totals.down,
        totals.pending
    );
    (segments, label, start_label, end_label)
}

fn recent_check_uptime_bar(
    history: &[CheckRecord],
    interval_seconds: u64,
    threshold_ms: Option<u64>,
    segment_count: usize,
    now_ms: i64,
) -> (Vec<UptimeSegment>, String, String, String) {
    if segment_count == 0 {
        return (Vec::new(), String::new(), String::new(), String::new());
    }

    let interval_ms = interval_millis(interval_seconds);
    let last_slot = schedule_slot(now_ms, interval_ms);
    let slot_count = segment_count.saturating_sub(1).min(i64::MAX as usize) as i64;
    let first_slot = last_slot.saturating_sub(slot_count);
    let mut records_by_slot = BTreeMap::<i64, &CheckRecord>::new();
    for record in history {
        let slot = schedule_slot(record.checked_at_ms, interval_ms);
        if slot < first_slot || slot > last_slot {
            continue;
        }
        let replace = records_by_slot
            .get(&slot)
            .is_none_or(|stored| record.checked_at_ms > stored.checked_at_ms);
        if replace {
            records_by_slot.insert(slot, record);
        }
    }

    let mut counts = PeriodCounts::default();
    let mut segments = Vec::with_capacity(segment_count);
    for index in 0..segment_count {
        let slot = first_slot.saturating_add(index.min(i64::MAX as usize) as i64);
        let expected_at_ms = slot.saturating_mul(interval_ms);
        let record = records_by_slot.get(&slot).copied();
        let (status_class, status_label) = record
            .map(|record| record_status(record, threshold_ms))
            .unwrap_or(("pending", "Pending"));
        match status_class {
            "up" => counts.operational += 1,
            "slow" => counts.degraded += 1,
            "down" => counts.down += 1,
            _ => counts.pending += 1,
        }
        segments.push(UptimeSegment {
            status_class: status_class.to_owned(),
            title: format!("{} · {status_label}", format_timestamp(expected_at_ms)),
        });
    }

    let label = format!(
        "Last {segment_count} checks: {} operational, {} degraded, {} down, {} pending",
        counts.operational, counts.degraded, counts.down, counts.pending
    );
    let start_label = format_duration_ago((segment_count as u64).saturating_mul(interval_seconds));
    let end_label = "Now".to_owned();
    (segments, label, start_label, end_label)
}

fn monitor_view(
    monitor: &MonitorConfig,
    latest: Option<&CheckRecord>,
    normal_percent: Option<f64>,
    normal_days: u32,
    bar_history: &[CheckRecord],
    samples_per_day: u32,
    now_ms: i64,
) -> MonitorView {
    let (kind, target) = match &monitor.target {
        MonitorTarget::Http { url, method, .. } => {
            ("HTTP/S", format!("{method} {}", redact_url_query(url)))
        }
        MonitorTarget::Ping { host } => ("Ping", host.clone()),
        MonitorTarget::Tcp { host, port, .. } => {
            let target = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            ("TCP", target)
        }
    };
    let (status_class, status_label, last_checked, response_time, message) = match latest {
        Some(record) => {
            let (status_class, status_label) =
                record_status(record, monitor.slow_response_threshold_ms);
            let (last_checked, response_time, message) = if record.is_pending {
                (
                    format!(
                        "No check recorded · {}",
                        format_timestamp(record.checked_at_ms)
                    ),
                    "—".to_owned(),
                    public_record_message(monitor, record),
                )
            } else {
                (
                    format_timestamp(record.checked_at_ms),
                    record
                        .latency_ms
                        .map(format_milliseconds)
                        .unwrap_or_else(|| "—".to_owned()),
                    public_record_message(monitor, record),
                )
            };
            (
                status_class.to_owned(),
                status_label.to_owned(),
                last_checked,
                response_time,
                message,
            )
        }
        None => (
            "pending".to_owned(),
            "Pending".to_owned(),
            "Not checked yet".to_owned(),
            "—".to_owned(),
            String::new(),
        ),
    };
    let (uptime_bar, uptime_bar_label, uptime_bar_start_label, uptime_bar_end_label) =
        monitor_uptime_bar(
            bar_history,
            monitor.interval_seconds,
            monitor.slow_response_threshold_ms,
            normal_days,
            samples_per_day,
            now_ms,
        );

    MonitorView {
        id: monitor.id.clone(),
        name: monitor.name.clone(),
        exclude_from_aggregate_status: monitor.exclude_from_aggregate_status,
        show_target: !monitor.hide_target,
        kind: kind.to_owned(),
        target: if monitor.hide_target {
            String::new()
        } else {
            target
        },
        interval_seconds: monitor.interval_seconds,
        response_threshold_label: monitor
            .slow_response_threshold_ms
            .map(|threshold| format!("> {}", format_milliseconds(threshold as f64)))
            .unwrap_or_else(|| "Disabled".to_owned()),
        status_class,
        status_label,
        last_checked,
        response_time,
        message,
        normal_label: format!("Availability ({normal_days}d)"),
        normal_percent: normal_percent
            .map(|value| format!("{value:.2}%"))
            .unwrap_or_else(|| "—".to_owned()),
        uptime_bar,
        uptime_bar_label,
        uptime_bar_start_label,
        uptime_bar_end_label,
    }
}

fn interval_millis(interval_seconds: u64) -> i64 {
    interval_seconds
        .max(1)
        .saturating_mul(1000)
        .min(i64::MAX as u64) as i64
}

fn schedule_slot(timestamp_ms: i64, interval_ms: i64) -> i64 {
    timestamp_ms.div_euclid(interval_ms)
}

fn scheduled_check_history(
    records: &[CheckRecord],
    interval_seconds: u64,
    now_ms: i64,
    limit: usize,
) -> Vec<CheckRecord> {
    if records.is_empty() || limit == 0 {
        return Vec::new();
    }

    let interval_ms = interval_millis(interval_seconds);
    let last_slot = schedule_slot(now_ms, interval_ms);
    let oldest_record_slot = records
        .iter()
        .map(|record| schedule_slot(record.checked_at_ms, interval_ms))
        .min()
        .unwrap_or(last_slot);
    if oldest_record_slot > last_slot {
        return Vec::new();
    }
    let first_slot = oldest_record_slot
        .max(last_slot.saturating_sub(limit.saturating_sub(1).min(i64::MAX as usize) as i64));
    let monitor_id = records[0].monitor_id.clone();
    let mut records_by_slot = BTreeMap::<i64, CheckRecord>::new();
    for record in records {
        let slot = schedule_slot(record.checked_at_ms, interval_ms);
        if slot < first_slot || slot > last_slot {
            continue;
        }
        let replace = records_by_slot
            .get(&slot)
            .is_none_or(|stored| record.checked_at_ms > stored.checked_at_ms);
        if replace {
            records_by_slot.insert(slot, record.clone());
        }
    }

    let mut scheduled = Vec::with_capacity(limit);
    for slot in (first_slot..=last_slot).rev() {
        if let Some(record) = records_by_slot.remove(&slot) {
            scheduled.push(record);
        } else {
            scheduled.push(CheckRecord {
                monitor_id: monitor_id.clone(),
                checked_at_ms: slot.saturating_mul(interval_ms),
                is_up: false,
                is_degraded: false,
                is_pending: true,
                latency_ms: None,
                http_timings: None,
                status_code: None,
                service_status_code: None,
                metrics: BTreeMap::new(),
                metric_units: BTreeMap::new(),
                message: MONITORING_UNAVAILABLE_MESSAGE.to_owned(),
                tcp_response: None,
                tcp_response_truncated: false,
            });
        }
    }
    scheduled
}

fn redact_url_query(raw: &str) -> String {
    url::Url::parse(raw)
        .map(|mut url| {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|_| raw.to_owned())
}

fn public_record_message(monitor: &MonitorConfig, record: &CheckRecord) -> String {
    if record.is_pending {
        return MONITORING_UNAVAILABLE_MESSAGE.to_owned();
    }

    if record.is_degraded {
        return if response_timing_exceeded(monitor, record) {
            "Response timing exceeded slow_ms".to_owned()
        } else {
            "Service reported degraded status".to_owned()
        };
    }

    if record.is_up {
        return match &monitor.target {
            MonitorTarget::Http { .. } => record
                .status_code
                .map(|code| format!("HTTP {code}"))
                .unwrap_or_else(|| "HTTP request succeeded".to_owned()),
            MonitorTarget::Ping { .. } => "ICMP reply received".to_owned(),
            MonitorTarget::Tcp { send_message, .. } => {
                if send_message.is_some() {
                    if record.tcp_response_truncated {
                        "TCP exchange succeeded; response capture was truncated".to_owned()
                    } else {
                        "TCP exchange succeeded".to_owned()
                    }
                } else {
                    "TCP connection succeeded".to_owned()
                }
            }
        };
    }

    match &monitor.target {
        MonitorTarget::Http {
            accepted_status_codes,
            ..
        } => match record.status_code {
            Some(code)
                if if accepted_status_codes.is_empty() {
                    (200..300).contains(&code)
                } else {
                    accepted_status_codes.contains(&code)
                } =>
            {
                "HTTP service reported a problem".to_owned()
            }
            Some(code) => format!("HTTP {code} was not accepted"),
            None => "HTTP request failed".to_owned(),
        },
        MonitorTarget::Ping { .. } => "Ping check failed".to_owned(),
        MonitorTarget::Tcp { send_message, .. } => {
            tcp_failure_message(monitor, record, send_message.is_some())
        }
    }
}

fn response_timing_exceeded(monitor: &MonitorConfig, record: &CheckRecord) -> bool {
    is_slow_response(record, monitor.slow_response_threshold_ms)
}

fn tcp_failure_message(monitor: &MonitorConfig, record: &CheckRecord, is_exchange: bool) -> String {
    let stored = record.message.as_str();
    if stored.contains("TCP DNS lookup failed") {
        return "TCP DNS lookup failed".to_owned();
    }
    if stored.contains("TCP DNS lookup returned no addresses") {
        return "TCP DNS lookup returned no addresses".to_owned();
    }
    if stored.contains("TCP message send failed") {
        return "TCP message send failed".to_owned();
    }
    if stored.contains("TCP response read failed") {
        return "TCP response read failed".to_owned();
    }
    if stored.contains("TCP response ended before response_end") {
        return "TCP response ended before response_end".to_owned();
    }
    if stored.contains("timed out") {
        return if is_exchange {
            "TCP exchange timed out".to_owned()
        } else {
            "TCP connection timed out".to_owned()
        };
    }

    let is_legacy_tcp_error = stored.contains("TCP exchange with ") && stored.contains(" failed: ")
        || stored.contains("TCP connection to ") && stored.contains(" failed: ");
    if is_legacy_tcp_error {
        return if is_exchange {
            "TCP exchange failed".to_owned()
        } else {
            "TCP connection failed".to_owned()
        };
    }

    if record.tcp_response.is_some() {
        "TCP service reported a problem".to_owned()
    } else if monitor.slow_response_threshold_ms.is_some_and(|threshold| {
        record
            .latency_ms
            .is_some_and(|latency| latency > threshold as f64)
    }) {
        "TCP response exceeded slow_ms".to_owned()
    } else if is_exchange {
        "TCP exchange failed".to_owned()
    } else {
        "TCP connection failed".to_owned()
    }
}

pub(super) fn japan_datetime(timestamp_ms: i64) -> Option<DateTime<FixedOffset>> {
    let japan_offset = FixedOffset::east_opt(9 * 60 * 60)?;
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .map(|date| date.with_timezone(&japan_offset))
}

fn format_timestamp(timestamp_ms: i64) -> String {
    japan_datetime(timestamp_ms)
        .map(|date| date.format("%Y-%m-%d %H:%M:%S JST").to_string())
        .unwrap_or_else(|| "—".to_owned())
}

fn format_duration_ago(seconds: u64) -> String {
    let units = [
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
        (1, "second"),
    ];
    let mut remaining = seconds;
    let mut parts = Vec::new();

    for (unit_seconds, unit_name) in units {
        let value = remaining / unit_seconds;
        if value > 0 || (parts.is_empty() && unit_seconds == 1) {
            parts.push(format!(
                "{value} {unit_name}{}",
                if value == 1 { "" } else { "s" }
            ));
            remaining %= unit_seconds;
            if parts.len() == 2 {
                break;
            }
        }
    }

    format!("{} ago", parts.join(" "))
}

fn error_page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>{title}</title><body><h1>{title}</h1><p>{message}</p><a href=\"/\">Back to dashboard</a></body></html>"
    )
}

fn render<T: Serialize>(name: &str, context: &T) -> Result<String, minijinja::Error> {
    let mut env = Environment::new();
    env.set_auto_escape_callback(minijinja::default_auto_escape_callback);
    env.add_template("base.html", include_str!("../templates/base.html"))?;
    env.add_template("style.css", include_str!("../templates/style.css"))?;
    env.add_template(
        "dashboard.html",
        include_str!("../templates/dashboard.html"),
    )?;
    env.add_template("detail.html", include_str!("../templates/detail.html"))?;
    env.get_template(name)?.render(Serde(context))
}

#[derive(Serialize)]
struct MonitorView {
    id: String,
    name: String,
    exclude_from_aggregate_status: bool,
    show_target: bool,
    kind: String,
    target: String,
    interval_seconds: u64,
    response_threshold_label: String,
    status_class: String,
    status_label: String,
    last_checked: String,
    response_time: String,
    message: String,
    normal_label: String,
    normal_percent: String,
    uptime_bar: Vec<UptimeSegment>,
    uptime_bar_label: String,
    uptime_bar_start_label: String,
    uptime_bar_end_label: String,
}

#[derive(Serialize)]
struct UptimeSegment {
    status_class: String,
    title: String,
}
