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

use chrono::{DateTime, Utc};
use kurosabi::{http::HttpMethod, server::tokio::KurosabiTokioServerBuilder};
use minijinja::{Environment, value::Serde};
use serde::Serialize;

mod charts;
use self::charts::{
    HttpTimingView, MetricGroupView, ResponseTimeChart, ResponseTimeStats, format_chart_timestamp,
    format_http_timing_summary, format_milliseconds, format_timing, http_timing_view,
    metric_graphs, response_time_chart, response_time_stats,
};

use crate::{
    config::{AppConfig, MonitorConfig, MonitorTarget, StatusMessages},
    store::{CheckRecord, Store},
};

const MAX_HISTORY: usize = 100;
const UPTIME_BAR_CHECKS: usize = 100;
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
                if conn.req.method() != &HttpMethod::GET {
                    return conn.set_status_code(405u16).html_body(error_page(
                        "405 Method Not Allowed",
                        "This request method is not supported.",
                    ));
                }

                let path_segments = conn.path_segs();
                let path: Vec<String> = path_segments
                    .iter()
                    .map(|segment| segment.to_string())
                    .collect();

                // Expose only application routes; workspace and configuration files are never served.
                match path.as_slice() {
                    [root] if root.is_empty() => match dashboard(&state).await {
                        Ok(html) => conn.html_body(html),
                        Err(error) => {
                            eprintln!("failed to render dashboard: {error}");
                            conn.set_status_code(500u16).html_body(error_page(
                                "500 Internal Server Error",
                                "Failed to load the monitoring dashboard.",
                            ))
                        }
                    },
                    [ping] if ping == "ping" => {
                        let ping_count = state.counters.ping.fetch_add(1, Ordering::Relaxed) + 1;
                        conn.text_body(format!(
                            "status.code 0\nserver.requests.unit requests\nserver.requests.total {total_count}\nserver.requests.ping {ping_count}\nserver.requests.healthz {}\nprocess.unit s\nprocess.uptime_seconds {:.1}\n",
                            state.counters.healthz.load(Ordering::Relaxed),
                            state.started_at.elapsed().as_secs_f64(),
                        ))
                    }
                    [healthz] if healthz == "healthz" => {
                        let healthz_count = state.counters.healthz.fetch_add(1, Ordering::Relaxed) + 1;
                        conn.text_body(format!(
                            "status.code 0\nserver.requests.unit requests\nserver.requests.total {total_count}\nserver.requests.ping {}\nserver.requests.healthz {healthz_count}\nprocess.unit s\nprocess.uptime_seconds {:.1}\n",
                            state.counters.ping.load(Ordering::Relaxed),
                            state.started_at.elapsed().as_secs_f64(),
                        ))
                    }
                    [fonts, font] if fonts == "fonts" && font == "UDEVGothicHSLG-Regular.woff2" => conn
                        .add_header("Content-Type", "font/woff2")
                        .add_header("Cache-Control", "public, max-age=86400")
                        .binary_body(include_bytes!("../fonts/UDEVGothicHSLG-Regular.woff2")),
                    [fonts, font] if fonts == "fonts" && font == "UDEVGothicHSLG-Bold.woff2" => conn
                        .add_header("Content-Type", "font/woff2")
                        .add_header("Cache-Control", "public, max-age=86400")
                        .binary_body(include_bytes!("../fonts/UDEVGothicHSLG-Bold.woff2")),
                    [monitors, id] if monitors == "monitors" => {
                        match monitor_detail(&state, id).await {
                            Ok(Some(html)) => conn.html_body(html),
                            Ok(None) => conn.redirect(NOT_FOUND_REDIRECT),
                            Err(error) => {
                                eprintln!("failed to render monitor {id}: {error}");
                                conn.set_status_code(500u16).html_body(error_page(
                                    "500 Internal Server Error",
                                    "Failed to load service history.",
                                ))
                            }
                        }
                    }
                    _ => conn.redirect(NOT_FOUND_REDIRECT),
                }
            }
        });

    println!("wk-status-371tti-net listening on http://{bind}:{port}");
    server.run().await
}

async fn dashboard(state: &WebState) -> io::Result<String> {
    let mut grouped_monitors = BTreeMap::<(String, bool), Vec<GroupMonitorData>>::new();
    let mut normal_count = 0usize;
    let mut down_count = 0usize;
    let mut slow_count = 0usize;
    let mut pending_count = 0usize;
    let mut status_total_count = 0usize;
    let now_ms = Utc::now().timestamp_millis();

    for monitor in &state.config.monitors {
        let records = state.store.history(&monitor.id, UPTIME_BAR_CHECKS).await?;
        let bar_history = scheduled_check_history(
            &records,
            monitor.interval_seconds,
            now_ms,
            UPTIME_BAR_CHECKS,
        );
        let latest = bar_history.first();
        let retention_days = state.config.storage.retention_days;
        let normal_percent = state
            .store
            .normal_percent(
                &monitor.id,
                retention_days,
                monitor.slow_response_threshold_ms,
            )
            .await?;
        let view = monitor_view(
            monitor,
            latest,
            normal_percent,
            retention_days,
            &bar_history,
        );
        if !monitor.exclude_from_aggregate_status {
            status_total_count += 1;
            match view.status_class.as_str() {
                "up" => normal_count += 1,
                "down" => down_count += 1,
                "slow" => slow_count += 1,
                _ => pending_count += 1,
            }
        }
        grouped_monitors
            .entry(monitor_group_key(&monitor.id))
            .or_default()
            .push(GroupMonitorData {
                view,
                history: bar_history,
                interval_seconds: monitor.interval_seconds,
                slow_response_threshold_ms: monitor.slow_response_threshold_ms,
                exclude_from_aggregate_status: monitor.exclude_from_aggregate_status,
            });
    }

    let mut groups = grouped_monitors
        .into_iter()
        .map(|((name, is_ungrouped), monitors)| {
            (
                is_ungrouped,
                monitor_group_view(name, is_ungrouped, monitors, &state.config.status_messages),
            )
        })
        .collect::<Vec<_>>();
    groups.sort_by_key(|(is_ungrouped, _)| *is_ungrouped);
    let groups = groups
        .into_iter()
        .map(|(_, group)| group)
        .collect::<Vec<_>>();
    let (overall_status_class, overall_status_label) = aggregate_status(
        state.config.monitors.len(),
        status_total_count,
        down_count,
        slow_count,
        pending_count,
        &state.config.status_messages,
    );

    let context = DashboardContext {
        app_name: state.config.server.name.clone(),
        page_title: "Status".to_owned(),
        generated_at: format_timestamp(Utc::now().timestamp_millis()),
        status_total_count,
        normal_count,
        down_count,
        slow_count,
        pending_count,
        overall_status_class: overall_status_class.to_owned(),
        overall_status_label: overall_status_label.to_owned(),
        groups,
    };
    render("dashboard.html", &context).map_err(io::Error::other)
}

fn monitor_group_key(monitor_id: &str) -> (String, bool) {
    monitor_id
        .rsplit_once('.')
        .map(|(group, _)| (group.to_owned(), false))
        .unwrap_or_else(|| ("Other".to_owned(), true))
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

fn is_slow_response(is_up: bool, latency_ms: Option<f64>, threshold_ms: Option<u64>) -> bool {
    is_up
        && threshold_ms
            .is_some_and(|threshold| latency_ms.is_some_and(|latency| latency > threshold as f64))
}

fn record_status(record: &CheckRecord, threshold_ms: Option<u64>) -> (&'static str, &'static str) {
    if record.is_pending {
        ("pending", "Pending")
    } else if !record.is_up {
        ("down", "Down")
    } else if record.is_degraded || is_slow_response(record.is_up, record.latency_ms, threshold_ms)
    {
        ("slow", "Degraded")
    } else {
        ("up", "Operational")
    }
}

fn monitor_group_view(
    name: String,
    is_ungrouped: bool,
    monitors: Vec<GroupMonitorData>,
    status_messages: &StatusMessages,
) -> MonitorGroupView {
    let (
        uptime_bar,
        uptime_bar_label,
        uptime_bar_caption,
        uptime_bar_start_label,
        uptime_bar_end_label,
    ) = group_uptime_bar(&monitors, status_messages);
    let total_count = monitors.len();
    let included_monitors: Vec<_> = monitors
        .iter()
        .filter(|monitor| !monitor.exclude_from_aggregate_status)
        .collect();
    let status_total_count = included_monitors.len();
    let normal_count = included_monitors
        .iter()
        .filter(|monitor| monitor.view.status_class == "up")
        .count();
    let down_count = included_monitors
        .iter()
        .filter(|monitor| monitor.view.status_class == "down")
        .count();
    let slow_count = included_monitors
        .iter()
        .filter(|monitor| monitor.view.status_class == "slow")
        .count();
    let pending_count = status_total_count - normal_count - down_count - slow_count;
    let (status_class, status_label) = aggregate_status(
        total_count,
        status_total_count,
        down_count,
        slow_count,
        pending_count,
        status_messages,
    );
    let monitors = monitors.into_iter().map(|monitor| monitor.view).collect();

    MonitorGroupView {
        name,
        show_group_status: !is_ungrouped,
        total_count,
        status_total_count,
        normal_count,
        down_count,
        slow_count,
        pending_count,
        status_class: status_class.to_owned(),
        status_label: status_label.to_owned(),
        uptime_bar,
        uptime_bar_label,
        uptime_bar_caption,
        uptime_bar_start_label,
        uptime_bar_end_label,
        monitors,
    }
}

fn group_uptime_bar(
    monitors: &[GroupMonitorData],
    status_messages: &StatusMessages,
) -> (Vec<UptimeSegment>, String, String, String, String) {
    let included_monitors: Vec<_> = monitors
        .iter()
        .filter(|monitor| !monitor.exclude_from_aggregate_status)
        .collect();
    let included_count = included_monitors.len();
    if included_count == 0 {
        return (
            Vec::new(),
            "No included services to summarize.".to_owned(),
            "No included services.".to_owned(),
            String::new(),
            String::new(),
        );
    }

    let empty_caption = "No checks yet.".to_owned();
    if included_monitors
        .iter()
        .any(|monitor| monitor.history.is_empty())
    {
        return (
            Vec::new(),
            "No checks yet.".to_owned(),
            empty_caption,
            String::new(),
            String::new(),
        );
    }

    let common_start_ms = included_monitors
        .iter()
        .filter_map(|monitor| monitor.history.last().map(|record| record.checked_at_ms))
        .max()
        .unwrap_or_else(|| Utc::now().timestamp_millis());
    let interval_seconds = included_monitors
        .iter()
        .map(|monitor| monitor.interval_seconds)
        .min()
        .unwrap_or(60);
    let interval_ms = interval_millis(interval_seconds);
    let now_ms = Utc::now().timestamp_millis();
    let latest_sample_slot = schedule_slot(now_ms, interval_ms);
    let mut first_available_slot = schedule_slot(common_start_ms, interval_ms);
    if common_start_ms.rem_euclid(interval_ms) != 0 {
        first_available_slot = first_available_slot.saturating_add(1);
    }
    let available_count = latest_sample_slot
        .saturating_sub(first_available_slot)
        .saturating_add(1)
        .max(0) as usize;
    if available_count == 0 {
        return (
            Vec::new(),
            "No checks yet.".to_owned(),
            empty_caption,
            String::new(),
            String::new(),
        );
    }
    let segment_count = available_count.min(UPTIME_BAR_CHECKS);
    let first_sample_slot = latest_sample_slot.saturating_sub(segment_count as i64 - 1);
    let first_sample_ms = first_sample_slot.saturating_mul(interval_ms);
    let mut normal_samples = 0usize;
    let mut down_samples = 0usize;
    let mut slow_samples = 0usize;
    let mut pending_samples = 0usize;
    let mut uptime_bar = Vec::with_capacity(segment_count);

    for sample_index in 0..segment_count {
        let sample_ms = first_sample_ms + sample_index as i64 * interval_ms;
        let mut normal_count = 0usize;
        let mut down_count = 0usize;
        let mut slow_count = 0usize;
        let mut pending_count = 0usize;

        for monitor in &included_monitors {
            let monitor_interval_ms = interval_millis(monitor.interval_seconds);
            let expected_slot = schedule_slot(sample_ms, monitor_interval_ms);
            let latest = monitor
                .history
                .iter()
                .filter(|record| {
                    schedule_slot(record.checked_at_ms, monitor_interval_ms) == expected_slot
                })
                .max_by_key(|record| record.checked_at_ms);
            match latest {
                None => pending_count += 1,
                Some(record) if record.is_pending => pending_count += 1,
                Some(record) if !record.is_up => down_count += 1,
                Some(record)
                    if is_slow_response(
                        record.is_up,
                        record.latency_ms,
                        monitor.slow_response_threshold_ms,
                    ) || record.is_degraded =>
                {
                    slow_count += 1;
                }
                Some(_) => normal_count += 1,
            }
        }

        let (status_class, _) = aggregate_status(
            monitors.len(),
            included_count,
            down_count,
            slow_count,
            pending_count,
            status_messages,
        );
        let status_label = compact_status_label(status_class);
        match status_class {
            "up" => normal_samples += 1,
            "down" => down_samples += 1,
            "slow" => slow_samples += 1,
            _ => pending_samples += 1,
        }
        uptime_bar.push(UptimeSegment {
            status_class: status_class.to_owned(),
            title: format!(
                "{} · {} · Operational: {}/{} · Degraded: {} · Down: {} · Pending: {}",
                format_timestamp(sample_ms),
                status_label,
                normal_count,
                included_count,
                slow_count,
                down_count,
                pending_count
            ),
        });
    }

    let count = uptime_bar.len();
    let uptime_bar_label = format!(
        "Last {} intervals: {} operational, {} degraded, {} down, {} pending",
        count, normal_samples, slow_samples, down_samples, pending_samples
    );
    let uptime_bar_caption = format!(
        "Last {} intervals · {} seconds each",
        count, interval_seconds
    );
    let uptime_bar_start_label = format_chart_timestamp(first_sample_ms);
    let last_sample_ms = first_sample_ms + (segment_count.saturating_sub(1) as i64) * interval_ms;
    let uptime_bar_end_label = format_chart_timestamp(last_sample_ms);
    (
        uptime_bar,
        uptime_bar_label,
        uptime_bar_caption,
        uptime_bar_start_label,
        uptime_bar_end_label,
    )
}

async fn monitor_detail(state: &WebState, id: &str) -> io::Result<Option<String>> {
    let Some(monitor) = state
        .config
        .monitors
        .iter()
        .find(|monitor| monitor.id == id)
    else {
        return Ok(None);
    };
    let records = state.store.history(&monitor.id, MAX_HISTORY).await?;
    let history = scheduled_check_history(
        &records,
        monitor.interval_seconds,
        Utc::now().timestamp_millis(),
        MAX_HISTORY,
    );
    let latest = history.first();
    let retention_days = state.config.storage.retention_days;
    let normal_percent = state
        .store
        .normal_percent(
            &monitor.id,
            retention_days,
            monitor.slow_response_threshold_ms,
        )
        .await?;
    let (response_time_stats, average_latency_ms) = response_time_stats(&history);
    let context = DetailContext {
        app_name: state.config.server.name.clone(),
        page_title: monitor.name.clone(),
        monitor: monitor_view(monitor, latest, normal_percent, retention_days, &history),
        response_chart: response_time_chart(
            &history,
            monitor.slow_response_threshold_ms,
            average_latency_ms,
        ),
        metric_groups: metric_graphs(&history),
        response_time_stats,
        is_http_monitor: matches!(&monitor.target, MonitorTarget::Http { .. }),
        http_timing: latest
            .filter(|record| !record.is_pending)
            .and_then(|record| record.http_timings.as_ref().map(http_timing_view)),
        is_tcp_monitor: matches!(&monitor.target, MonitorTarget::Tcp { .. }),
        tcp_exchange_enabled: matches!(
            &monitor.target,
            MonitorTarget::Tcp {
                send_message: Some(_),
                ..
            }
        ),
        has_tcp_check: !records.is_empty(),
        latest_tcp_response: records
            .first()
            .and_then(|record| record.tcp_response.as_deref().map(format_tcp_response)),
        latest_tcp_response_truncated: records
            .first()
            .is_some_and(|record| record.tcp_response_truncated),
        history: history
            .iter()
            .map(|record| history_view(record, monitor.slow_response_threshold_ms))
            .collect(),
    };
    render("detail.html", &context)
        .map(Some)
        .map_err(io::Error::other)
}

fn monitor_view(
    monitor: &MonitorConfig,
    latest: Option<&CheckRecord>,
    normal_percent: Option<f64>,
    normal_days: u32,
    bar_history: &[CheckRecord],
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
                    record.message.clone(),
                )
            } else {
                (
                    format_timestamp(record.checked_at_ms),
                    record
                        .latency_ms
                        .map(format_milliseconds)
                        .unwrap_or_else(|| "—".to_owned()),
                    record.message.clone(),
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
    let normal_count = bar_history
        .iter()
        .filter(|record| record_status(record, monitor.slow_response_threshold_ms).0 == "up")
        .count();
    let slow_count = bar_history
        .iter()
        .filter(|record| record_status(record, monitor.slow_response_threshold_ms).0 == "slow")
        .count();
    let down_count = bar_history
        .iter()
        .filter(|record| record_status(record, monitor.slow_response_threshold_ms).0 == "down")
        .count();
    let pending_count = bar_history
        .iter()
        .filter(|record| record_status(record, monitor.slow_response_threshold_ms).0 == "pending")
        .count();
    let uptime_bar_label = format!(
        "Last {} intervals: {} operational, {} degraded, {} down, {} pending",
        bar_history.len(),
        normal_count,
        slow_count,
        down_count,
        pending_count
    );
    let uptime_bar_start_label = bar_history
        .last()
        .map(|record| format_chart_timestamp(record.checked_at_ms))
        .unwrap_or_default();
    let uptime_bar_end_label = bar_history
        .first()
        .map(|record| format_chart_timestamp(record.checked_at_ms))
        .unwrap_or_default();
    let uptime_bar = bar_history
        .iter()
        .rev()
        .map(|record| uptime_segment(record, monitor.slow_response_threshold_ms))
        .collect();

    MonitorView {
        id: monitor.id.clone(),
        name: monitor.name.clone(),
        exclude_from_aggregate_status: monitor.exclude_from_aggregate_status,
        show_target: !monitor.hide_target,
        kind: kind.to_owned(),
        target,
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

fn uptime_segment(record: &CheckRecord, threshold_ms: Option<u64>) -> UptimeSegment {
    let (status_class, status_label) = record_status(record, threshold_ms);
    let response_time = record
        .latency_ms
        .map(format_milliseconds)
        .unwrap_or_else(|| "—".to_owned());
    let mut title = format!(
        "{} · {} · {}",
        format_timestamp(record.checked_at_ms),
        status_label,
        response_time
    );
    if record.is_pending {
        title.push_str(" · ");
        title.push_str(&record.message);
    }
    UptimeSegment {
        status_class: status_class.to_owned(),
        title: title
            + &record
                .http_timings
                .as_ref()
                .map(|timings| {
                    format!(
                        " · {} · Total {}",
                        format_http_timing_summary(timings),
                        format_timing(timings.total_ms)
                    )
                })
                .unwrap_or_default(),
    }
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

fn history_view(record: &CheckRecord, threshold_ms: Option<u64>) -> HistoryView {
    let (status_class, status_label) = record_status(record, threshold_ms);
    HistoryView {
        checked_at: format_timestamp(record.checked_at_ms),
        status_class: status_class.to_owned(),
        status_label: status_label.to_owned(),
        response_time: record
            .latency_ms
            .map(format_milliseconds)
            .unwrap_or_else(|| "—".to_owned()),
        status_code: record
            .status_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "—".to_owned()),
        message: record.message.clone(),
        tcp_response: record.tcp_response.as_deref().map(format_tcp_response),
        tcp_response_truncated: record.tcp_response_truncated,
    }
}

fn format_tcp_response(response: &[u8]) -> String {
    match std::str::from_utf8(response) {
        Ok(response) => response.to_owned(),
        Err(_) => {
            let preview = response
                .iter()
                .take(256)
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            if response.len() > 256 {
                format!(
                    "Binary response ({} bytes; first 256 bytes in hex): {preview} …",
                    response.len()
                )
            } else {
                format!(
                    "Binary response ({} bytes in hex): {preview}",
                    response.len()
                )
            }
        }
    }
}

fn format_timestamp(timestamp_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .map(|date| date.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "—".to_owned())
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
struct DashboardContext {
    app_name: String,
    page_title: String,
    generated_at: String,
    status_total_count: usize,
    normal_count: usize,
    down_count: usize,
    slow_count: usize,
    pending_count: usize,
    overall_status_class: String,
    overall_status_label: String,
    groups: Vec<MonitorGroupView>,
}

#[derive(Serialize)]
struct MonitorGroupView {
    name: String,
    show_group_status: bool,
    total_count: usize,
    status_total_count: usize,
    normal_count: usize,
    down_count: usize,
    slow_count: usize,
    pending_count: usize,
    status_class: String,
    status_label: String,
    uptime_bar: Vec<UptimeSegment>,
    uptime_bar_label: String,
    uptime_bar_caption: String,
    uptime_bar_start_label: String,
    uptime_bar_end_label: String,
    monitors: Vec<MonitorView>,
}

struct GroupMonitorData {
    view: MonitorView,
    history: Vec<CheckRecord>,
    interval_seconds: u64,
    slow_response_threshold_ms: Option<u64>,
    exclude_from_aggregate_status: bool,
}

#[derive(Serialize)]
struct DetailContext {
    app_name: String,
    page_title: String,
    monitor: MonitorView,
    response_chart: ResponseTimeChart,
    metric_groups: Vec<MetricGroupView>,
    response_time_stats: ResponseTimeStats,
    is_http_monitor: bool,
    http_timing: Option<HttpTimingView>,
    is_tcp_monitor: bool,
    tcp_exchange_enabled: bool,
    has_tcp_check: bool,
    latest_tcp_response: Option<String>,
    latest_tcp_response_truncated: bool,
    history: Vec<HistoryView>,
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

#[derive(Serialize)]
struct HistoryView {
    checked_at: String,
    status_class: String,
    status_label: String,
    response_time: String,
    status_code: String,
    message: String,
    tcp_response: Option<String>,
    tcp_response_truncated: bool,
}
