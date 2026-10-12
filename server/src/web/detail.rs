use std::io;

use chrono::Utc;
use serde::Serialize;

use super::charts::{
    HttpTimingView, MetricGroupView, ResponseTimeChart, ResponseTimeStats, http_timing_view,
    metric_graphs, response_time_chart, response_time_stats,
};
use super::{
    MAX_HISTORY, MILLIS_PER_DAY, MonitorView, UptimeSegment, WebState, format_milliseconds,
    format_timestamp, monitor_view, normal_percent_for_history, public_record_message,
    recent_check_uptime_bar, record_status, render, scheduled_check_history,
};
use crate::{
    config::{MonitorConfig, MonitorTarget},
    store::CheckRecord,
};

pub(super) async fn monitor_detail(state: &WebState, id: &str) -> io::Result<Option<String>> {
    let Some(context) = monitor_detail_data(state, id).await? else {
        return Ok(None);
    };
    render("detail.html", &context)
        .map(Some)
        .map_err(io::Error::other)
}

pub(super) async fn monitor_detail_data(
    state: &WebState,
    id: &str,
) -> io::Result<Option<DetailContext>> {
    let Some(monitor) = state
        .config
        .monitors
        .iter()
        .find(|monitor| monitor.id == id)
    else {
        return Ok(None);
    };
    let now_ms = Utc::now().timestamp_millis();
    let retention_days = state.config.storage.retention_days;
    let window_end_ms = now_ms;
    let retention_start_ms =
        window_end_ms.saturating_sub(i64::from(retention_days) * MILLIS_PER_DAY);
    let records = state
        .store
        .history_range(&monitor.id, retention_start_ms, window_end_ms)
        .await?;
    let history = scheduled_check_history(&records, monitor.interval_seconds, now_ms, MAX_HISTORY);
    let latest = history.first();
    let normal_percent = normal_percent_for_history(&records, monitor.slow_response_threshold_ms);
    let (response_time_stats, _) = response_time_stats(&records);
    let recent_check_count =
        (retention_days as usize).saturating_mul(state.config.status_bar.samples_per_day as usize);
    let (
        recent_checks_bar,
        recent_checks_bar_label,
        recent_checks_bar_start_label,
        recent_checks_bar_end_label,
    ) = recent_check_uptime_bar(
        &records,
        monitor.interval_seconds,
        monitor.slow_response_threshold_ms,
        recent_check_count,
        now_ms,
    );
    let context = DetailContext {
        app_name: state.config.server.name.clone(),
        page_title: monitor.name.clone(),
        status_data_url: format!("/api/monitors/{}/status", monitor.id),
        monitor: monitor_view(
            monitor,
            latest,
            normal_percent,
            retention_days,
            &records,
            state.config.status_bar.samples_per_day,
            now_ms,
        ),
        recent_checks_bar,
        recent_checks_bar_label,
        recent_checks_bar_start_label,
        recent_checks_bar_end_label,
        recent_checks_count_label: format!("Last {recent_check_count} checks"),
        chart_retention_start_ms: retention_start_ms,
        chart_window_end_ms: window_end_ms,
        chart_data_url: format!("/api/monitors/{}/charts", monitor.id),
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
            .map(|record| history_view(monitor, record))
            .collect(),
    };
    Ok(Some(context))
}

pub(super) async fn monitor_chart_data(
    state: &WebState,
    id: &str,
    window_end_ms: i64,
    requested_start_ms: i64,
    requested_end_ms: i64,
) -> io::Result<Option<MonitorChartData>> {
    let Some(monitor) = state
        .config
        .monitors
        .iter()
        .find(|monitor| monitor.id == id)
    else {
        return Ok(None);
    };

    let now_ms = Utc::now().timestamp_millis();
    let window_end_ms = window_end_ms.min(now_ms);
    let retention_start_ms = window_end_ms
        .saturating_sub(i64::from(state.config.storage.retention_days) * MILLIS_PER_DAY);
    let range_start_ms = requested_start_ms.max(retention_start_ms);
    let range_end_ms = requested_end_ms.min(window_end_ms);
    if range_start_ms >= range_end_ms {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chart range is outside retained history",
        ));
    }

    let records = state
        .store
        .history_range(&monitor.id, range_start_ms, range_end_ms)
        .await?;
    Ok(Some(MonitorChartData {
        range_start_ms,
        range_end_ms,
        response_chart: response_time_chart(
            &records,
            monitor.slow_response_threshold_ms,
            matches!(&monitor.target, MonitorTarget::Http { .. }),
            monitor.interval_seconds,
            range_start_ms,
            range_end_ms,
        ),
        metric_groups: metric_graphs(
            &records,
            monitor.interval_seconds,
            range_start_ms,
            range_end_ms,
        ),
    }))
}

fn history_view(monitor: &MonitorConfig, record: &CheckRecord) -> HistoryView {
    let (status_class, status_label) = record_status(record, monitor.slow_response_threshold_ms);
    HistoryView {
        checked_at_ms: record.checked_at_ms,
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
        message: public_record_message(monitor, record),
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

#[derive(Serialize)]
pub(super) struct DetailContext {
    app_name: String,
    page_title: String,
    status_data_url: String,
    monitor: MonitorView,
    recent_checks_bar: Vec<UptimeSegment>,
    recent_checks_bar_label: String,
    recent_checks_bar_start_label: String,
    recent_checks_bar_end_label: String,
    recent_checks_count_label: String,
    chart_retention_start_ms: i64,
    chart_window_end_ms: i64,
    chart_data_url: String,
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
#[serde(rename_all = "camelCase")]
pub(super) struct MonitorChartData {
    range_start_ms: i64,
    range_end_ms: i64,
    response_chart: ResponseTimeChart,
    metric_groups: Vec<MetricGroupView>,
}

#[derive(Serialize)]
struct HistoryView {
    checked_at_ms: i64,
    checked_at: String,
    status_class: String,
    status_label: String,
    response_time: String,
    status_code: String,
    message: String,
    tcp_response: Option<String>,
    tcp_response_truncated: bool,
}
