use std::{collections::BTreeMap, io};

use chrono::Utc;
use serde::Serialize;

use super::{
    MAX_HISTORY, MILLIS_PER_DAY, MonitorView, PeriodCounts, UptimeSegment, WebState,
    aggregate_status, compact_status_label, format_duration_ago, format_timestamp,
    monitor_period_status, monitor_view, normal_percent_for_history, render,
    scheduled_check_history, status_bar_period,
};
use crate::{config::StatusMessages, store::CheckRecord};

pub(super) async fn dashboard(state: &WebState) -> io::Result<String> {
    let context = dashboard_data(state).await?;
    render("dashboard.html", &context).map_err(io::Error::other)
}

pub(super) async fn dashboard_data(state: &WebState) -> io::Result<DashboardContext> {
    let mut grouped_monitors = BTreeMap::<(String, bool), Vec<GroupMonitorData>>::new();
    let mut normal_count = 0usize;
    let mut down_count = 0usize;
    let mut slow_count = 0usize;
    let mut pending_count = 0usize;
    let mut status_total_count = 0usize;
    let now_ms = Utc::now().timestamp_millis();
    let retention_days = state.config.storage.retention_days;
    let samples_per_day = state.config.status_bar.samples_per_day;
    let retention_start_ms = now_ms.saturating_sub(i64::from(retention_days) * MILLIS_PER_DAY);

    for (config_index, monitor) in state.config.monitors.iter().enumerate() {
        let records = state
            .store
            .history_range(&monitor.id, retention_start_ms, now_ms)
            .await?;
        let current_history =
            scheduled_check_history(&records, monitor.interval_seconds, now_ms, MAX_HISTORY);
        let latest = current_history.first();
        let normal_percent =
            normal_percent_for_history(&records, monitor.slow_response_threshold_ms);
        let view = monitor_view(
            monitor,
            latest,
            normal_percent,
            retention_days,
            &records,
            samples_per_day,
            now_ms,
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
                history: records,
                interval_seconds: monitor.interval_seconds,
                slow_response_threshold_ms: monitor.slow_response_threshold_ms,
                exclude_from_aggregate_status: monitor.exclude_from_aggregate_status,
                order: monitor.order,
                config_index,
            });
    }

    let mut groups = grouped_monitors
        .into_iter()
        .map(|((name, is_ungrouped), mut monitors)| {
            monitors.sort_by_key(|monitor| {
                (
                    monitor.order.is_none(),
                    monitor.order.unwrap_or_default(),
                    monitor.config_index,
                )
            });
            let group_order = monitors.iter().filter_map(|monitor| monitor.order).min();
            (
                (
                    is_ungrouped,
                    group_order.is_none(),
                    group_order.unwrap_or_default(),
                    name.clone(),
                ),
                monitor_group_view(
                    name,
                    is_ungrouped,
                    monitors,
                    &state.config.status_messages,
                    retention_days,
                    samples_per_day,
                    now_ms,
                ),
            )
        })
        .collect::<Vec<_>>();
    groups.sort_by(|(left_order, _), (right_order, _)| left_order.cmp(right_order));
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
        status_data_url: "/api/status".to_owned(),
        status_total_count,
        normal_count,
        down_count,
        slow_count,
        pending_count,
        overall_status_class: overall_status_class.to_owned(),
        overall_status_label: overall_status_label.to_owned(),
        groups,
    };
    Ok(context)
}

fn monitor_group_key(monitor_id: &str) -> (String, bool) {
    monitor_id
        .rsplit_once('.')
        .map(|(group, _)| (group.to_owned(), false))
        .unwrap_or_else(|| ("Other".to_owned(), true))
}

fn monitor_group_view(
    name: String,
    is_ungrouped: bool,
    monitors: Vec<GroupMonitorData>,
    status_messages: &StatusMessages,
    retention_days: u32,
    samples_per_day: u32,
    now_ms: i64,
) -> MonitorGroupView {
    let (
        uptime_bar,
        uptime_bar_label,
        uptime_bar_caption,
        uptime_bar_start_label,
        uptime_bar_end_label,
    ) = group_uptime_bar(
        &monitors,
        status_messages,
        retention_days,
        samples_per_day,
        now_ms,
    );
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
    retention_days: u32,
    samples_per_day: u32,
    now_ms: i64,
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

    let retention_start_ms = now_ms.saturating_sub(i64::from(retention_days) * MILLIS_PER_DAY);
    let total_segments = retention_days as usize * samples_per_day as usize;
    let monitor_buckets = included_monitors
        .iter()
        .map(|monitor| {
            monitor_period_status(monitor, retention_start_ms, retention_days, samples_per_day)
        })
        .collect::<Vec<_>>();
    let mut counts = PeriodCounts::default();
    let mut uptime_bar = Vec::with_capacity(total_segments);

    for segment_index in 0..total_segments {
        let (sample_start_ms, sample_end_ms) =
            status_bar_period(retention_start_ms, segment_index, samples_per_day);
        let mut operational_count = 0usize;
        let mut degraded_count = 0usize;
        let mut down_count = 0usize;
        let mut pending_count = 0usize;
        for buckets in &monitor_buckets {
            match buckets[segment_index].status_class() {
                "up" => operational_count += 1,
                "slow" => degraded_count += 1,
                "down" => down_count += 1,
                _ => pending_count += 1,
            }
        }
        let (status_class, _) = aggregate_status(
            monitors.len(),
            included_count,
            down_count,
            degraded_count,
            pending_count,
            status_messages,
        );
        match status_class {
            "up" => counts.operational += 1,
            "down" => counts.down += 1,
            "slow" => counts.degraded += 1,
            _ => counts.pending += 1,
        }
        uptime_bar.push(UptimeSegment {
            status_class: status_class.to_owned(),
            title: format!(
                "{} – {} · {} · Operational: {}/{} · Degraded: {} · Down: {} · Pending: {}",
                format_timestamp(sample_start_ms),
                format_timestamp(sample_end_ms),
                compact_status_label(status_class),
                operational_count,
                included_count,
                degraded_count,
                down_count,
                pending_count
            ),
        });
    }

    let count = uptime_bar.len();
    let uptime_bar_label = format!(
        "Last {retention_days} days: {count} samples, {} operational, {} degraded, {} down, {} pending",
        counts.operational, counts.degraded, counts.down, counts.pending
    );
    let uptime_bar_caption = format!("Last {retention_days} days · {samples_per_day} samples/day");
    let uptime_bar_start_label = uptime_bar
        .first()
        .map(|_| format_duration_ago(u64::from(retention_days).saturating_mul(86_400)))
        .unwrap_or_default();
    let uptime_bar_end_label = if uptime_bar.is_empty() {
        String::new()
    } else {
        "Now".to_owned()
    };
    (
        uptime_bar,
        uptime_bar_label,
        uptime_bar_caption,
        uptime_bar_start_label,
        uptime_bar_end_label,
    )
}

#[derive(Serialize)]
pub(super) struct DashboardContext {
    app_name: String,
    page_title: String,
    status_data_url: String,
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

pub(super) struct GroupMonitorData {
    view: MonitorView,
    pub(super) history: Vec<CheckRecord>,
    pub(super) interval_seconds: u64,
    pub(super) slow_response_threshold_ms: Option<u64>,
    exclude_from_aggregate_status: bool,
    order: Option<i64>,
    config_index: usize,
}
