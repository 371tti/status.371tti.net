use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

use super::record_status;
use crate::{http_probe::HttpTimings, store::CheckRecord};

const MAX_METRIC_CHARTS: usize = 128;
const METRIC_SERIES_COLORS: usize = 8;

struct MetricHistory<'a> {
    records: &'a [CheckRecord],
    aliases: &'a BTreeMap<String, BTreeSet<String>>,
}

pub(super) fn response_time_chart(
    history: &[CheckRecord],
    threshold_ms: Option<u64>,
    include_http_phases: bool,
    interval_seconds: u64,
    range_start_ms: i64,
    range_end_ms: i64,
) -> ResponseTimeChart {
    let all_chronological: Vec<&CheckRecord> = history.iter().rev().collect();
    let (path_start, path_end) = chart_path_range(&all_chronological, range_start_ms, range_end_ms);
    let chronological = &all_chronological[path_start..path_end];
    let (_, average_latency_ms) = response_time_stats_for(
        history
            .iter()
            .filter(|record| (range_start_ms..range_end_ms).contains(&record.checked_at_ms)),
    );
    let max_latency = all_chronological
        .iter()
        .filter(|record| (range_start_ms..range_end_ms).contains(&record.checked_at_ms))
        .filter_map(|record| record.latency_ms)
        .filter(|latency| latency.is_finite() && *latency >= 0.0)
        .fold(0.0_f64, f64::max);
    let overlay_keys = response_overlay_keys(history);
    let max_overlay_ms = all_chronological
        .iter()
        .filter(|record| (range_start_ms..range_end_ms).contains(&record.checked_at_ms))
        .flat_map(|record| {
            overlay_keys
                .iter()
                .filter_map(|key| record.metrics.get(key))
                .copied()
        })
        .filter(|value| value.is_finite() && *value >= 0.0)
        .fold(0.0_f64, f64::max);
    let chart_max = max_latency
        .max(max_overlay_ms)
        .max(average_latency_ms.unwrap_or(0.0))
        .max(threshold_ms.unwrap_or_default() as f64);
    let step = response_chart_step(chart_max);
    let axis_max = step * 4.0;

    let mut data_point_count = 0;
    let mut has_phase_data = false;
    let mut response_data = Vec::with_capacity(chronological.len());
    let mut phase_data: Option<[Vec<serde_json::Value>; 4]> = include_http_phases
        .then(|| std::array::from_fn(|_| Vec::with_capacity(chronological.len())));
    let mut previous_timestamp_ms = None;
    let max_gap_ms = interval_seconds.saturating_mul(2000).min(i64::MAX as u64) as i64;
    for record in chronological {
        if let Some(previous) = previous_timestamp_ms {
            let gap = record.checked_at_ms.saturating_sub(previous);
            if gap > max_gap_ms {
                let gap_timestamp_ms = previous.saturating_add(gap / 2);
                response_data.push(json!({ "x": gap_timestamp_ms, "y": null }));
                if let Some(phase_data) = &mut phase_data {
                    for phase in phase_data {
                        phase.push(json!({ "x": gap_timestamp_ms, "y": null }));
                    }
                }
            }
        }
        let latency = record
            .latency_ms
            .filter(|latency| latency.is_finite() && *latency >= 0.0);
        let http_timing = if include_http_phases {
            record.http_timings.as_ref().map(http_timing_view)
        } else {
            None
        };
        let phase_values = if include_http_phases {
            record.http_timings.as_ref().and_then(chart_phase_values)
        } else {
            None
        };
        has_phase_data |= phase_values.is_some();
        if latency.is_some() && (range_start_ms..range_end_ms).contains(&record.checked_at_ms) {
            data_point_count += 1;
        }
        response_data.push(json!({
            "x": record.checked_at_ms,
            "y": latency,
            "statusClass": record_status(record, threshold_ms).0,
            "inRange": (range_start_ms..range_end_ms).contains(&record.checked_at_ms),
            "httpTiming": http_timing,
        }));
        if let Some(phase_data) = &mut phase_data {
            for (index, phase) in phase_data.iter_mut().enumerate() {
                phase.push(json!({
                    "x": record.checked_at_ms,
                    "y": phase_values.map(|values| values[index]),
                }));
            }
        }
        previous_timestamp_ms = Some(record.checked_at_ms);
    }

    let mut datasets = vec![
        json!({ "label": "Response", "kind": "line", "colorIndex": 0, "data": response_data }),
    ];
    if let Some(phase_data) = phase_data.as_ref().filter(|_| has_phase_data) {
        datasets.extend([
            json!({ "label": "DNS", "kind": "bar", "colorIndex": 1, "data": phase_data[0] }),
            json!({ "label": "TCP", "kind": "bar", "colorIndex": 2, "data": phase_data[1] }),
            json!({ "label": "TLS", "kind": "bar", "colorIndex": 3, "data": phase_data[2] }),
            json!({ "label": "HTTP", "kind": "bar", "colorIndex": 4, "data": phase_data[3] }),
        ]);
    }
    for (index, key) in overlay_keys.iter().enumerate() {
        let mut data = Vec::with_capacity(chronological.len());
        let mut previous_timestamp_ms = None;
        for record in chronological {
            if let Some(previous) = previous_timestamp_ms {
                let gap = record.checked_at_ms.saturating_sub(previous);
                if gap > max_gap_ms {
                    data.push(json!({
                        "x": previous.saturating_add(gap / 2),
                        "y": null,
                    }));
                }
            }
            let mut point = json!({
                "x": record.checked_at_ms,
                "y": record.metrics.get(key).filter(|value| value.is_finite()),
            });
            if !(range_start_ms..range_end_ms).contains(&record.checked_at_ms) {
                point["inRange"] = json!(false);
            }
            data.push(point);
            previous_timestamp_ms = Some(record.checked_at_ms);
        }
        datasets.push(json!({
            "label": key.strip_prefix("response.").unwrap_or(key),
            "key": key.strip_prefix("response.").unwrap_or(key),
            "kind": "overlay",
            "colorIndex": (index + 1) % METRIC_SERIES_COLORS,
            "unit": "ms",
            "data": data,
        }));
    }

    let chart_data = json!({
        "kind": "response",
        "xMin": range_start_ms,
        "xMax": range_end_ms,
        "yMin": 0.0,
        "yMax": axis_max,
        "average": average_latency_ms,
        "threshold": threshold_ms.map(|threshold| threshold as f64),
        "datasets": datasets
    });
    let phase_description = if has_phase_data {
        "Stacked bars show the DNS, TCP, TLS, and HTTP breakdown."
    } else {
        ""
    };
    ResponseTimeChart {
        aria_label: format!(
            "Response time trend. {}X axis: check time. Y axis: milliseconds. Average response time: {}. Slow response threshold: {}. {} data points.",
            phase_description,
            average_latency_ms
                .map(format_milliseconds)
                .unwrap_or_else(|| "not available".to_owned()),
            threshold_ms
                .map(|threshold| format_milliseconds(threshold as f64))
                .unwrap_or_else(|| "disabled".to_owned()),
            data_point_count,
        ),
        data_point_count,
        chart_data,
    }
}

pub(super) fn metric_graphs(
    history: &[CheckRecord],
    interval_seconds: u64,
    range_start_ms: i64,
    range_end_ms: i64,
) -> Vec<MetricGroupView> {
    let mut keys = BTreeSet::new();
    let mut metric_aliases = BTreeMap::<String, BTreeSet<String>>::new();
    for record in history {
        for key in record.metrics.keys() {
            if is_response_overlay_key(key) {
                continue;
            }
            let canonical_key = canonical_metric_key(key);
            if keys.len() < MAX_METRIC_CHARTS || keys.contains(&canonical_key) {
                keys.insert(canonical_key.clone());
                if canonical_key != *key {
                    metric_aliases
                        .entry(canonical_key)
                        .or_default()
                        .insert(key.clone());
                }
            }
        }
    }

    let mut grouped_keys = BTreeMap::<String, Vec<String>>::new();
    let mut standalone_keys = Vec::new();
    for key in keys {
        if let Some((group, _)) = key.rsplit_once('.') {
            grouped_keys.entry(group.to_owned()).or_default().push(key);
        } else {
            standalone_keys.push(key);
        }
    }

    let metric_history = MetricHistory {
        records: history,
        aliases: &metric_aliases,
    };
    let mut groups = BTreeMap::<String, Vec<MetricChartView>>::new();
    for (name, mut keys) in grouped_keys {
        keys.sort_by(|left, right| {
            let left_label = left.rsplit('.').next().unwrap_or(left);
            let right_label = right.rsplit('.').next().unwrap_or(right);
            match (left_label, right_label) {
                ("all", "all") => std::cmp::Ordering::Equal,
                ("all", _) => std::cmp::Ordering::Less,
                (_, "all") => std::cmp::Ordering::Greater,
                _ => match (left_label.parse::<usize>(), right_label.parse::<usize>()) {
                    (Ok(left_index), Ok(right_index)) => left_index
                        .cmp(&right_index)
                        .then_with(|| left_label.cmp(right_label)),
                    (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                    (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                    (Err(_), Err(_)) => left_label.cmp(right_label),
                },
            }
        });
        let unit = latest_metric_unit(&name, history);
        if let Some(chart) = metric_chart(
            &name,
            &keys,
            &metric_history,
            &unit,
            interval_seconds,
            range_start_ms,
            range_end_ms,
        ) {
            groups.entry(name).or_default().push(chart);
        }
    }
    for key in standalone_keys {
        let unit = latest_metric_unit("Metrics", history);
        if let Some(chart) = metric_chart(
            "Metrics",
            std::slice::from_ref(&key),
            &metric_history,
            &unit,
            interval_seconds,
            range_start_ms,
            range_end_ms,
        ) {
            groups.entry("Metrics".to_owned()).or_default().push(chart);
        }
    }

    groups
        .into_iter()
        .map(|(name, charts)| {
            let unit = latest_metric_unit(&name, history);
            MetricGroupView { name, unit, charts }
        })
        .collect::<Vec<_>>()
}

fn response_overlay_keys(history: &[CheckRecord]) -> BTreeSet<String> {
    history
        .iter()
        .flat_map(|record| record.metrics.keys())
        .filter(|key| is_response_overlay_key(key))
        .cloned()
        .collect()
}

fn is_response_overlay_key(key: &str) -> bool {
    key.starts_with("response.") && key != "response.ms"
}

fn latest_metric_unit(group: &str, history: &[CheckRecord]) -> String {
    history
        .iter()
        .filter_map(|record| {
            record.metric_units.iter().find_map(|(raw_group, unit)| {
                canonical_metric_unit_groups(raw_group)
                    .iter()
                    .any(|canonical| canonical == group)
                    .then_some((record.checked_at_ms, unit))
            })
        })
        .max_by_key(|(checked_at_ms, _)| *checked_at_ms)
        .map(|(_, unit)| unit.clone())
        .unwrap_or_default()
}

fn metric_chart(
    group: &str,
    keys: &[String],
    history: &MetricHistory<'_>,
    unit: &str,
    interval_seconds: u64,
    range_start_ms: i64,
    range_end_ms: i64,
) -> Option<MetricChartView> {
    let chronological_all: Vec<&CheckRecord> = history.records.iter().rev().collect();
    let (path_start, path_end) = chart_path_range(&chronological_all, range_start_ms, range_end_ms);
    let chronological = &chronological_all[path_start..path_end];
    let metric_keys: Vec<&str> = keys
        .iter()
        .map(String::as_str)
        .filter(|key| {
            chronological_all.iter().any(|record| {
                metric_value(record, key, history.aliases).is_some_and(|value| value.is_finite())
            })
        })
        .collect();
    if metric_keys.is_empty() {
        return None;
    }

    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for key in &metric_keys {
        for record in &chronological_all {
            if let Some(value) =
                metric_value(record, key, history.aliases).filter(|value| value.is_finite())
            {
                minimum = minimum.min(*value);
                maximum = maximum.max(*value);
            }
        }
    }
    let (axis_min, axis_max) = metric_axis_bounds(minimum, maximum);
    let mut chart_datasets = Vec::with_capacity(metric_keys.len());
    for (series_index, key) in metric_keys.into_iter().enumerate() {
        let color_index = series_index % METRIC_SERIES_COLORS;
        let mut chart_data = Vec::with_capacity(chronological.len());
        let mut previous_chart_timestamp_ms = None;
        let max_gap_ms = interval_seconds.saturating_mul(2000).min(i64::MAX as u64) as i64;
        for record in chronological {
            if let Some(previous) = previous_chart_timestamp_ms {
                let gap = record.checked_at_ms.saturating_sub(previous);
                if gap > max_gap_ms {
                    chart_data.push(json!({
                        "x": previous.saturating_add(gap / 2),
                        "y": null,
                    }));
                }
            }
            previous_chart_timestamp_ms = Some(record.checked_at_ms);
            let value =
                metric_value(record, key, history.aliases).filter(|value| value.is_finite());
            let mut point = json!({
                "x": record.checked_at_ms,
                "y": value,
            });
            if !(range_start_ms..range_end_ms).contains(&record.checked_at_ms) {
                point["inRange"] = json!(false);
            }
            chart_data.push(point);
        }
        chart_datasets.push(json!({
            "label": key.rsplit('.').next().unwrap_or(key),
            "key": key,
            "unit": unit,
            "colorIndex": color_index,
            "data": chart_data,
        }));
    }

    let unit_description = if unit.is_empty() {
        String::new()
    } else {
        format!(" ({unit})")
    };
    let chart_data = json!({
        "kind": "metric",
        "xMin": range_start_ms,
        "xMax": range_end_ms,
        "yMin": axis_min,
        "yMax": axis_max,
        "datasets": chart_datasets,
    });
    Some(MetricChartView {
        aria_label: format!(
            "{group}{unit_description} metrics trend with a shared vertical scale; each colored line represents one metric"
        ),
        chart_data,
    })
}

fn metric_axis_bounds(minimum: f64, maximum: f64) -> (f64, f64) {
    let spread = maximum - minimum;
    let scale = if spread.is_finite() && spread > 0.0 {
        spread
    } else {
        minimum.abs().max(maximum.abs())
    };
    if scale == 0.0 {
        return (0.0, 1.0);
    }

    let step = nice_chart_step(scale / 4.0);
    if !step.is_finite() || step <= 0.0 {
        return (minimum, maximum);
    }

    let mut axis_min = (minimum / step).floor() * step;
    let mut axis_max = (maximum / step).ceil() * step;
    if axis_min == axis_max {
        axis_min -= step;
        axis_max += step;
        if minimum >= 0.0 && axis_min < 0.0 {
            axis_min = 0.0;
        }
    }

    if !axis_min.is_finite() || !axis_max.is_finite() || axis_min >= axis_max {
        (minimum, maximum)
    } else {
        (axis_min, axis_max)
    }
}

fn chart_path_range(
    chronological: &[&CheckRecord],
    range_start_ms: i64,
    range_end_ms: i64,
) -> (usize, usize) {
    let first_in_range =
        chronological.partition_point(|record| record.checked_at_ms < range_start_ms);
    let first_after_range =
        chronological.partition_point(|record| record.checked_at_ms < range_end_ms);
    (
        first_in_range.saturating_sub(1),
        first_after_range.saturating_add(1).min(chronological.len()),
    )
}

fn canonical_metric_key(key: &str) -> String {
    match key {
        "cpu.percent.usage" => return "cpu.percent.all".to_owned(),
        "cpu.percent.iowait" => return "cpu.iowait.all".to_owned(),
        _ => {}
    }
    if let Some(rest) = key.strip_prefix("cpu.core.") {
        if let Some((core, metric)) = rest.split_once(".percent.")
            && core.bytes().all(|byte| byte.is_ascii_digit())
        {
            return match metric {
                "usage" => format!("cpu.percent.{core}"),
                "iowait" => format!("cpu.iowait.{core}"),
                _ => key.to_owned(),
            };
        }
        if rest.starts_with("percent.") || rest.starts_with("iowait.") {
            return format!("cpu.{rest}");
        }
    }
    let parts = key.split('.').collect::<Vec<_>>();
    if parts.len() == 4
        && parts[0] == "network"
        && matches!(parts[2], "bytes" | "packets" | "errors" | "drops")
        && matches!(parts[3], "rx" | "tx")
    {
        return format!("network.{}.{}.{}", parts[2], parts[3], parts[1]);
    }
    key.to_owned()
}

fn metric_value<'a>(
    record: &'a CheckRecord,
    canonical_key: &str,
    metric_aliases: &BTreeMap<String, BTreeSet<String>>,
) -> Option<&'a f64> {
    record.metrics.get(canonical_key).or_else(|| {
        metric_aliases
            .get(canonical_key)?
            .iter()
            .find_map(|alias| record.metrics.get(alias))
    })
}

fn canonical_metric_unit_groups(group: &str) -> Vec<String> {
    if matches!(
        group,
        "cpu.percent" | "cpu.iowait" | "cpu.core.percent" | "cpu.core.iowait"
    ) {
        return [
            "cpu.percent",
            "cpu.iowait",
            "cpu.core.percent",
            "cpu.core.iowait",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
    }
    if let Some(rest) = group.strip_prefix("cpu.core.")
        && let Some((core, metric)) = rest.split_once('.')
        && core.bytes().all(|byte| byte.is_ascii_digit())
        && matches!(metric, "percent" | "iowait")
    {
        return [
            "cpu.percent",
            "cpu.iowait",
            "cpu.core.percent",
            "cpu.core.iowait",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
    }
    let parts = group.split('.').collect::<Vec<_>>();
    if parts.len() == 3
        && parts[0] == "network"
        && matches!(parts[2], "bytes" | "packets" | "errors" | "drops")
    {
        return vec![
            format!("network.{}.rx", parts[2]),
            format!("network.{}.tx", parts[2]),
        ];
    }
    vec![group.to_owned()]
}

pub(super) fn response_time_stats(history: &[CheckRecord]) -> (ResponseTimeStats, Option<f64>) {
    response_time_stats_for(history.iter())
}

fn response_time_stats_for<'a>(
    history: impl IntoIterator<Item = &'a CheckRecord>,
) -> (ResponseTimeStats, Option<f64>) {
    let mut count = 0usize;
    let mut total_ms = 0.0_f64;
    let mut maximum_ms = 0.0_f64;
    for latency in history
        .into_iter()
        .filter_map(|record| record.latency_ms)
        .filter(|latency| latency.is_finite() && *latency >= 0.0)
    {
        count += 1;
        total_ms += latency;
        maximum_ms = maximum_ms.max(latency);
    }

    if count == 0 {
        return (
            ResponseTimeStats {
                average: "—".to_owned(),
                maximum: "—".to_owned(),
            },
            None,
        );
    }

    let average_ms = total_ms / count as f64;
    (
        ResponseTimeStats {
            average: format_milliseconds(average_ms),
            maximum: format_milliseconds(maximum_ms),
        },
        Some(average_ms),
    )
}

fn response_chart_step(max_latency_ms: f64) -> f64 {
    let rough_step = (max_latency_ms / 4.0).max(0.01);
    nice_chart_step(rough_step)
}

fn nice_chart_step(rough_step: f64) -> f64 {
    let magnitude = 10_f64.powf(rough_step.log10().floor());
    let normalized = rough_step / magnitude;
    let nice_step = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 2.5 {
        2.5
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice_step * magnitude
}

fn format_si_number(value: f64, fractional_digits_below_1000: usize) -> String {
    const PREFIXES: [&str; 11] = ["", "k", "M", "G", "T", "P", "E", "Z", "Y", "R", "Q"];

    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return "0".to_owned();
    }

    let magnitude = value.abs();
    if magnitude < 1000.0 {
        return trim_decimal_zeros(format!("{value:.fractional_digits_below_1000$}"));
    }

    let mut prefix_index = (magnitude.log10() / 3.0).floor() as usize;
    if prefix_index >= PREFIXES.len() {
        return format!("{value:.3e}");
    }

    let mut scaled = value / 1000_f64.powi(prefix_index as i32);
    let mut precision = si_precision(scaled.abs());
    let rounded = (scaled * 10_f64.powi(precision as i32)).round() / 10_f64.powi(precision as i32);
    if rounded.abs() >= 1000.0 && prefix_index + 1 < PREFIXES.len() {
        prefix_index += 1;
        scaled = value / 1000_f64.powi(prefix_index as i32);
        precision = si_precision(scaled.abs());
    }

    format!(
        "{}{}",
        trim_decimal_zeros(format!("{scaled:.precision$}")),
        PREFIXES[prefix_index]
    )
}

fn si_precision(scaled_magnitude: f64) -> usize {
    if scaled_magnitude < 10.0 {
        2
    } else if scaled_magnitude < 100.0 {
        1
    } else {
        0
    }
}

fn trim_decimal_zeros(value: String) -> String {
    if value.contains('.') {
        value.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        value
    }
}

pub(super) fn format_milliseconds(value: f64) -> String {
    format!("{} ms", format_si_number(value, 1))
}

fn chart_phase_values(timings: &HttpTimings) -> Option<[f64; 4]> {
    let dns = if timings.dns_lookup_attempted {
        timings.dns_ms?
    } else {
        0.0
    };
    let tcp = timings.tcp_connect_ms?;
    let tls = if timings.tls_handshake_attempted {
        timings.tls_handshake_ms?
    } else {
        0.0
    };
    let http = timings.request_to_headers_ms?;
    let values = [dns, tcp, tls, http];
    values
        .iter()
        .all(|value| value.is_finite() && *value >= 0.0)
        .then_some(values)
}

pub(super) fn http_timing_view(timings: &HttpTimings) -> HttpTimingView {
    HttpTimingView {
        dns: if timings.dns_lookup_attempted {
            format_timing(timings.dns_ms)
        } else {
            "Not needed".to_owned()
        },
        tcp_connect: format_timing(timings.tcp_connect_ms),
        tls_handshake: if timings.tls_handshake_attempted {
            format_timing(timings.tls_handshake_ms)
        } else {
            "Not applicable".to_owned()
        },
        request_to_headers: format_timing(timings.request_to_headers_ms),
        total: format_timing(timings.total_ms),
    }
}

pub(super) fn format_timing(value: Option<f64>) -> String {
    value
        .map(format_milliseconds)
        .unwrap_or_else(|| "—".to_owned())
}

#[derive(Serialize)]
pub(super) struct ResponseTimeStats {
    average: String,
    maximum: String,
}

#[derive(Serialize)]
pub(super) struct HttpTimingView {
    dns: String,
    tcp_connect: String,
    tls_handshake: String,
    request_to_headers: String,
    total: String,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResponseTimeChart {
    aria_label: String,
    data_point_count: usize,
    chart_data: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MetricGroupView {
    name: String,
    unit: String,
    charts: Vec<MetricChartView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricChartView {
    aria_label: String,
    chart_data: serde_json::Value,
}
