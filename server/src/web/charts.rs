use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use super::{format_timestamp, record_status};
use crate::{http_probe::HttpTimings, store::CheckRecord};

const RESPONSE_CHART_MIN_WIDTH: usize = 908;
const RESPONSE_CHART_POINT_SPACING: usize = 16;
const RESPONSE_CHART_EDGE: f64 = 8.0;
const RESPONSE_CHART_TOP: f64 = 18.0;
const RESPONSE_CHART_BOTTOM: f64 = 224.0;
const METRIC_CHART_TOP: f64 = 12.0;
const METRIC_CHART_BOTTOM: f64 = 126.0;
const MAX_METRIC_CHARTS: usize = 128;
const METRIC_SERIES_COLORS: usize = 8;

pub(super) fn response_time_chart(
    history: &[CheckRecord],
    threshold_ms: Option<u64>,
    average_latency_ms: Option<f64>,
) -> ResponseTimeChart {
    let chronological: Vec<&CheckRecord> = history.iter().rev().collect();
    let Some(first) = chronological.first() else {
        return ResponseTimeChart::default();
    };
    let Some(last) = chronological.last() else {
        return ResponseTimeChart::default();
    };

    let max_latency = chronological
        .iter()
        .filter_map(|record| record.latency_ms)
        .filter(|latency| latency.is_finite() && *latency >= 0.0)
        .fold(0.0_f64, f64::max);
    let has_latency = chronological.iter().any(|record| {
        record
            .latency_ms
            .is_some_and(|latency| latency.is_finite() && latency >= 0.0)
    });
    if !has_latency {
        return ResponseTimeChart::default();
    }

    let chart_max = max_latency
        .max(average_latency_ms.unwrap_or(0.0))
        .max(threshold_ms.unwrap_or_default() as f64);
    let step = response_chart_step(chart_max);
    let axis_max = step * 4.0;
    let plot_width = RESPONSE_CHART_MIN_WIDTH
        .max(chronological.len().saturating_sub(1) * RESPONSE_CHART_POINT_SPACING);
    let plot_width_f64 = plot_width as f64;
    let plot_right = plot_width_f64 - RESPONSE_CHART_EDGE;
    let plot_span = plot_right - RESPONSE_CHART_EDGE;
    let plot_height = RESPONSE_CHART_BOTTOM - RESPONSE_CHART_TOP;
    let first_ms = first.checked_at_ms;
    let span_ms = last.checked_at_ms.saturating_sub(first_ms).max(1) as f64;
    let x_at = |timestamp_ms: i64| {
        RESPONSE_CHART_EDGE + (timestamp_ms.saturating_sub(first_ms) as f64 / span_ms) * plot_span
    };
    let y_at = |latency_ms: f64| RESPONSE_CHART_BOTTOM - (latency_ms / axis_max) * plot_height;

    let ticks = (0..=4)
        .map(|index| {
            let value = axis_max * (4 - index) as f64 / 4.0;
            let y = RESPONSE_CHART_TOP + plot_height * index as f64 / 4.0;
            ResponseChartTick {
                y: chart_coordinate(y),
                label_y: chart_coordinate(y + 4.0),
                value: format_chart_number(value),
            }
        })
        .collect();

    let mut points = Vec::new();
    let mut line_paths = Vec::new();
    let mut current_path = String::new();
    for record in &chronological {
        let latency = record
            .latency_ms
            .filter(|latency| latency.is_finite() && *latency >= 0.0);
        let Some(latency) = latency else {
            if !current_path.is_empty() {
                line_paths.push(ResponseChartLine {
                    path_data: std::mem::take(&mut current_path),
                });
            }
            continue;
        };

        let x = x_at(record.checked_at_ms);
        let y = y_at(latency);
        if current_path.is_empty() {
            current_path = format!("M {} {}", chart_coordinate(x), chart_coordinate(y));
        } else {
            current_path.push_str(&format!(
                " L {} {}",
                chart_coordinate(x),
                chart_coordinate(y)
            ));
        }
        points.push(ResponseChartPoint {
            x: chart_coordinate(x),
            y: chart_coordinate(y),
            status_class: record_status(record, threshold_ms).0.to_owned(),
            checked_at: format_timestamp(record.checked_at_ms),
            aria_label: format!(
                "{} · Last response time: {} · {} · Select to view timing breakdown",
                format_timestamp(record.checked_at_ms),
                format_milliseconds(latency),
                record_status(record, threshold_ms).1
            ),
            http_timing: record.http_timings.as_ref().map(http_timing_view),
            phase_segments: record
                .http_timings
                .as_ref()
                .and_then(chart_phase_values)
                .map(|values| response_chart_phase_segments(x, values, &y_at))
                .unwrap_or_default(),
        });
    }
    if !current_path.is_empty() {
        line_paths.push(ResponseChartLine {
            path_data: current_path,
        });
    }

    let average_y = average_latency_ms.map(|average| chart_coordinate(y_at(average)));
    let threshold_y = threshold_ms.and_then(|threshold| {
        let threshold = threshold as f64;
        (threshold <= axis_max).then(|| chart_coordinate(y_at(threshold)))
    });
    let phase_description = if points.iter().any(|point| !point.phase_segments.is_empty()) {
        "Stacked bars show the DNS, TCP, TLS, and HTTP breakdown."
    } else {
        ""
    };
    ResponseTimeChart {
        aria_label: format!(
            "Response time trend. {}X axis: check time. Y axis: milliseconds. Average response time: {}. Slow response threshold: {}. {} data points.",
            phase_description,
            format_milliseconds(average_latency_ms.unwrap_or_default()),
            threshold_ms
                .map(|threshold| format_milliseconds(threshold as f64))
                .unwrap_or_else(|| "disabled".to_owned()),
            points.len(),
        ),
        start_label: format_chart_timestamp(first_ms),
        end_label: format_chart_timestamp(last.checked_at_ms),
        start_timestamp_ms: first_ms,
        end_timestamp_ms: last.checked_at_ms,
        plot_width,
        average_y,
        threshold_y,
        threshold_label: threshold_ms
            .map(|threshold| format_chart_number(threshold as f64))
            .unwrap_or_default(),
        ticks,
        line_paths,
        points,
    }
}

pub(super) fn metric_graphs(history: &[CheckRecord]) -> Vec<MetricGroupView> {
    let mut keys = BTreeSet::new();
    for record in history {
        for key in record.metrics.keys() {
            keys.insert(canonical_metric_key(key));
            if keys.len() == MAX_METRIC_CHARTS {
                break;
            }
        }
        if keys.len() == MAX_METRIC_CHARTS {
            break;
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
        if let Some(chart) = metric_chart(&name, &keys, history, &unit) {
            groups.entry(name).or_default().push(chart);
        }
    }
    for key in standalone_keys {
        let unit = latest_metric_unit("Metrics", history);
        if let Some(chart) = metric_chart("Metrics", std::slice::from_ref(&key), history, &unit) {
            groups.entry("Metrics".to_owned()).or_default().push(chart);
        }
    }

    groups
        .into_iter()
        .map(|(name, charts)| {
            let unit = latest_metric_unit(&name, history);
            MetricGroupView { name, unit, charts }
        })
        .collect()
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
    history: &[CheckRecord],
    unit: &str,
) -> Option<MetricChartView> {
    let chronological: Vec<&CheckRecord> = history.iter().rev().collect();
    let first = chronological.first()?;
    let last = chronological.last()?;
    let metric_keys: Vec<&str> = keys
        .iter()
        .map(String::as_str)
        .filter(|key| {
            chronological
                .iter()
                .any(|record| metric_value(record, key).is_some_and(|value| value.is_finite()))
        })
        .collect();
    if metric_keys.is_empty() {
        return None;
    }

    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for key in &metric_keys {
        for record in &chronological {
            if let Some(value) = metric_value(record, key).filter(|value| value.is_finite()) {
                minimum = minimum.min(*value);
                maximum = maximum.max(*value);
            }
        }
    }
    let spread = maximum - minimum;
    let padding = if spread == 0.0 {
        maximum.abs().max(1.0) * 0.08
    } else {
        spread * 0.08
    };
    let axis_min = minimum - padding;
    let axis_max = maximum + padding;
    let plot_width = RESPONSE_CHART_MIN_WIDTH
        .max(chronological.len().saturating_sub(1) * RESPONSE_CHART_POINT_SPACING);
    let plot_span = plot_width as f64 - 2.0 * RESPONSE_CHART_EDGE;
    let plot_height = METRIC_CHART_BOTTOM - METRIC_CHART_TOP;
    let first_ms = first.checked_at_ms;
    let span_ms = last.checked_at_ms.saturating_sub(first_ms).max(1) as f64;
    let x_at = |timestamp_ms: i64| {
        RESPONSE_CHART_EDGE + (timestamp_ms.saturating_sub(first_ms) as f64 / span_ms) * plot_span
    };
    let y_at = |value: f64| {
        METRIC_CHART_BOTTOM - ((value - axis_min) / (axis_max - axis_min)) * plot_height
    };
    let ticks = (0..=4)
        .map(|index| {
            let fraction = (4 - index) as f64 / 4.0;
            let value = axis_min + (axis_max - axis_min) * fraction;
            let y = METRIC_CHART_TOP + plot_height * index as f64 / 4.0;
            MetricChartTick {
                y: chart_coordinate(y),
                label_y: chart_coordinate(y + 4.0),
                value: format_chart_number(value),
            }
        })
        .collect();

    let mut series = Vec::with_capacity(metric_keys.len());
    for (series_index, key) in metric_keys.into_iter().enumerate() {
        let color_index = series_index % METRIC_SERIES_COLORS;
        let mut points = Vec::new();
        let mut line_paths = Vec::new();
        let mut current_path = String::new();
        for record in &chronological {
            let Some(value) = metric_value(record, key).filter(|value| value.is_finite()) else {
                if !current_path.is_empty() {
                    line_paths.push(MetricChartLine {
                        path_data: std::mem::take(&mut current_path),
                    });
                }
                continue;
            };
            let x = x_at(record.checked_at_ms);
            let y = y_at(*value);
            if current_path.is_empty() {
                current_path = format!("M {} {}", chart_coordinate(x), chart_coordinate(y));
            } else {
                current_path.push_str(&format!(
                    " L {} {}",
                    chart_coordinate(x),
                    chart_coordinate(y)
                ));
            }
            let checked_at = format_timestamp(record.checked_at_ms);
            let formatted_value = format_chart_number(*value);
            let display_value = if unit.is_empty() {
                formatted_value
            } else {
                format!("{formatted_value} {unit}")
            };
            points.push(MetricChartPoint {
                x: chart_coordinate(x),
                y: chart_coordinate(y),
                color_index,
                timestamp_ms: record.checked_at_ms,
                checked_at: checked_at.clone(),
                value: display_value.clone(),
                aria_label: format!("{checked_at} · {key}: {display_value}"),
            });
        }
        if !current_path.is_empty() {
            line_paths.push(MetricChartLine {
                path_data: current_path,
            });
        }
        series.push(MetricChartSeries {
            key: key.to_owned(),
            label: key.rsplit('.').next().unwrap_or(key).to_owned(),
            color_index,
            line_paths,
            points,
        });
    }

    let unit_description = if unit.is_empty() {
        String::new()
    } else {
        format!(" ({unit})")
    };
    Some(MetricChartView {
        aria_label: format!(
            "{group}{unit_description} metrics trend with a shared vertical scale; each colored line represents one metric"
        ),
        start_label: format_chart_timestamp(first_ms),
        end_label: format_chart_timestamp(last.checked_at_ms),
        start_timestamp_ms: first_ms,
        end_timestamp_ms: last.checked_at_ms,
        plot_width,
        ticks,
        series,
    })
}

fn canonical_metric_key(key: &str) -> String {
    match key {
        "cpu.percent.usage" => return "cpu.percent.all".to_owned(),
        "cpu.percent.iowait" => return "cpu.iowait.all".to_owned(),
        _ => {}
    }
    if let Some(rest) = key.strip_prefix("cpu.core.") {
        if let Some((core, metric)) = rest.split_once(".percent.") {
            if core.bytes().all(|byte| byte.is_ascii_digit()) {
                return match metric {
                    "usage" => format!("cpu.percent.{core}"),
                    "iowait" => format!("cpu.iowait.{core}"),
                    _ => key.to_owned(),
                };
            }
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

fn metric_value<'a>(record: &'a CheckRecord, canonical_key: &str) -> Option<&'a f64> {
    record.metrics.get(canonical_key).or_else(|| {
        record
            .metrics
            .iter()
            .find_map(|(key, value)| (canonical_metric_key(key) == canonical_key).then_some(value))
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
    if let Some(rest) = group.strip_prefix("cpu.core.") {
        if let Some((core, metric)) = rest.split_once('.') {
            if core.bytes().all(|byte| byte.is_ascii_digit())
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
        }
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
    let mut count = 0usize;
    let mut total_ms = 0.0_f64;
    let mut maximum_ms = 0.0_f64;
    for latency in history
        .iter()
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

fn chart_coordinate(value: f64) -> String {
    format!("{value:.2}")
}

fn format_chart_number(value: f64) -> String {
    format_si_number(value, 6)
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

pub(super) fn format_chart_timestamp(timestamp_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .map(|date| date.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".to_owned())
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

fn response_chart_phase_segments(
    x: f64,
    phase_values: [f64; 4],
    y_at: &impl Fn(f64) -> f64,
) -> Vec<ResponseChartPhaseSegment> {
    let phases = ["dns", "tcp", "tls", "http"];
    let mut elapsed_ms = 0.0;
    phase_values
        .into_iter()
        .zip(phases)
        .filter_map(|(phase_ms, phase)| {
            let previous_ms = elapsed_ms;
            elapsed_ms += phase_ms;
            let y = y_at(elapsed_ms);
            let bottom_y = y_at(previous_ms);
            let height = bottom_y - y;
            (height > 0.0).then(|| ResponseChartPhaseSegment {
                x: chart_coordinate(x - 3.5),
                y: chart_coordinate(y),
                width: "7".to_owned(),
                height: chart_coordinate(height),
                phase: phase.to_owned(),
            })
        })
        .collect()
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

pub(super) fn format_http_timing_summary(timings: &HttpTimings) -> String {
    let dns = if timings.dns_lookup_attempted {
        format_timing(timings.dns_ms)
    } else {
        "Not needed".to_owned()
    };
    let tls = if timings.tls_handshake_attempted {
        format_timing(timings.tls_handshake_ms)
    } else {
        "Not applicable".to_owned()
    };
    format!(
        "DNS {} · TCP {} · TLS {} · HTTP {}",
        dns,
        format_timing(timings.tcp_connect_ms),
        tls,
        format_timing(timings.request_to_headers_ms),
    )
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
pub(super) struct ResponseTimeChart {
    aria_label: String,
    start_label: String,
    end_label: String,
    start_timestamp_ms: i64,
    end_timestamp_ms: i64,
    plot_width: usize,
    average_y: Option<String>,
    threshold_y: Option<String>,
    threshold_label: String,
    ticks: Vec<ResponseChartTick>,
    line_paths: Vec<ResponseChartLine>,
    points: Vec<ResponseChartPoint>,
}

#[derive(Serialize)]
struct ResponseChartTick {
    y: String,
    label_y: String,
    value: String,
}

#[derive(Serialize)]
struct ResponseChartLine {
    path_data: String,
}

#[derive(Serialize)]
struct ResponseChartPoint {
    x: String,
    y: String,
    status_class: String,
    checked_at: String,
    aria_label: String,
    http_timing: Option<HttpTimingView>,
    phase_segments: Vec<ResponseChartPhaseSegment>,
}

#[derive(Serialize)]
struct ResponseChartPhaseSegment {
    x: String,
    y: String,
    width: String,
    height: String,
    phase: String,
}

#[derive(Serialize)]
pub(super) struct MetricGroupView {
    name: String,
    unit: String,
    charts: Vec<MetricChartView>,
}

#[derive(Serialize)]
struct MetricChartView {
    aria_label: String,
    start_label: String,
    end_label: String,
    start_timestamp_ms: i64,
    end_timestamp_ms: i64,
    plot_width: usize,
    ticks: Vec<MetricChartTick>,
    series: Vec<MetricChartSeries>,
}

#[derive(Serialize)]
struct MetricChartSeries {
    key: String,
    label: String,
    color_index: usize,
    line_paths: Vec<MetricChartLine>,
    points: Vec<MetricChartPoint>,
}

#[derive(Serialize)]
struct MetricChartTick {
    y: String,
    label_y: String,
    value: String,
}

#[derive(Serialize)]
struct MetricChartLine {
    path_data: String,
}

#[derive(Serialize)]
struct MetricChartPoint {
    x: String,
    y: String,
    color_index: usize,
    timestamp_ms: i64,
    checked_at: String,
    value: String,
    aria_label: String,
}
