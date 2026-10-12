use std::{
    collections::BTreeMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

use chrono::Utc;
use http::Method;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};
use tracing::{debug, error, info};

use crate::{
    config::{AppConfig, MonitorConfig, MonitorTarget, ResponseCodeStatus},
    http_probe::{self, HttpProbeClient, HttpTimings},
    store::Store,
};

const MAX_METRIC_COUNT: usize = 512;
const MAX_METRIC_KEY_BYTES: usize = 128;
const MAX_METRIC_UNIT_COUNT: usize = 512;
const MAX_METRIC_UNIT_BYTES: usize = 64;

#[derive(Clone, Debug)]
pub struct CheckResult {
    pub monitor_id: String,
    pub checked_at_ms: i64,
    pub is_up: bool,
    pub is_degraded: bool,
    pub latency_ms: Option<f64>,
    pub http_timings: Option<HttpTimings>,
    pub status_code: Option<u16>,
    pub service_status_code: Option<i64>,
    pub metrics: BTreeMap<String, f64>,
    pub metric_units: BTreeMap<String, String>,
    pub message: String,
    pub tcp_response: Option<Vec<u8>>,
    pub tcp_response_truncated: bool,
}

#[derive(Debug, Default)]
struct TargetResult {
    is_up: bool,
    response_received: bool,
    latency_ms: Option<f64>,
    http_timings: Option<HttpTimings>,
    status_code: Option<u16>,
    telemetry: Option<Vec<u8>>,
    message: String,
    tcp_response: Option<Vec<u8>>,
    tcp_response_truncated: bool,
}

#[derive(Clone, Copy)]
enum TcpProbeFailure {
    DnsLookup,
    NoAddresses,
    Connect,
    Send,
    Read,
    ResponseTerminator,
}

impl TcpProbeFailure {
    fn public_message(self) -> &'static str {
        match self {
            Self::DnsLookup => "TCP DNS lookup failed",
            Self::NoAddresses => "TCP DNS lookup returned no addresses",
            Self::Connect => "TCP connection failed",
            Self::Send => "TCP message send failed",
            Self::Read => "TCP response read failed",
            Self::ResponseTerminator => "TCP response ended before response_end",
        }
    }
}

#[derive(Clone, Copy)]
enum PingProbeFailure {
    DnsLookup,
    NoAddresses,
    IcmpEcho,
}

impl PingProbeFailure {
    fn public_message(self) -> &'static str {
        match self {
            Self::DnsLookup => "Ping DNS lookup failed",
            Self::NoAddresses => "Ping DNS lookup returned no addresses",
            Self::IcmpEcho => "ICMP echo failed",
        }
    }
}

pub fn start_tasks(config: &AppConfig, store: Store) {
    for monitor in config.monitors.iter().cloned() {
        let monitor_type = match &monitor.target {
            MonitorTarget::Http { .. } => "http",
            MonitorTarget::Ping { .. } => "ping",
            MonitorTarget::Tcp { .. } => "tcp",
        };
        info!(
            monitor_id = %monitor.id,
            monitor_type,
            interval_seconds = monitor.interval_seconds,
            timeout_seconds = monitor.timeout_seconds,
            slow_ms = ?monitor.slow_response_threshold_ms,
            excluded_from_status = monitor.exclude_from_aggregate_status,
            "monitor scheduled"
        );
        let monitor_store = store.clone();
        tokio::spawn(async move {
            let http_probe_client = matches!(&monitor.target, MonitorTarget::Http { .. })
                .then(http_probe::build_client);
            if let Some(Err(error)) = &http_probe_client {
                error!(
                    monitor_id = %monitor.id,
                    error = %error,
                    "failed to initialize HTTP probe"
                );
            }
            loop {
                sleep(delay_until_next_check(monitor.interval_seconds)).await;
                debug!(monitor_id = %monitor.id, "monitor check started");
                let result = run_check(&monitor, http_probe_client.as_ref()).await;
                let status = if result.is_degraded {
                    "degraded"
                } else if result.is_up {
                    "operational"
                } else {
                    "down"
                };
                info!(
                    monitor_id = %result.monitor_id,
                    status,
                    checked_at_ms = result.checked_at_ms,
                    latency_ms = ?result.latency_ms,
                    http_status = ?result.status_code,
                    service_status_code = ?result.service_status_code,
                    http_timings = ?result.http_timings,
                    metric_count = result.metrics.len(),
                    message = %result.message,
                    "monitor check completed"
                );
                if let Err(error) = monitor_store.insert_check(&result).await {
                    error!(
                        monitor_id = %monitor.id,
                        error = %error,
                        "failed to save monitor check"
                    );
                }
            }
        });
    }

    let retention_store = store;
    tokio::spawn(async move {
        let mut ticks = interval(Duration::from_secs(24 * 60 * 60));
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            match retention_store.delete_expired().await {
                Ok(()) => info!("history retention cleanup completed"),
                Err(error) => error!(error = %error, "failed to clean expired history"),
            }
        }
    });
}

fn delay_until_next_check(interval_seconds: u64) -> Duration {
    let interval_ms = interval_seconds
        .max(1)
        .saturating_mul(1000)
        .min(i64::MAX as u64) as i64;
    let remainder_ms = Utc::now().timestamp_millis().rem_euclid(interval_ms);
    Duration::from_millis(((interval_ms - remainder_ms) % interval_ms) as u64)
}

async fn run_check(
    monitor: &MonitorConfig,
    http_probe_client: Option<&Result<HttpProbeClient, String>>,
) -> CheckResult {
    let mut target = check_target(monitor, http_probe_client).await;
    let telemetry = target
        .telemetry
        .as_deref()
        .map(parse_key_values)
        .unwrap_or_default();
    let slow = monitor.slow_response_threshold_ms.is_some_and(|threshold| {
        target
            .latency_ms
            .is_some_and(|latency| latency > threshold as f64)
    });
    let service_status_code = telemetry.status_code.unwrap_or({
        if telemetry.invalid_status_code || !target.is_up || !target.response_received {
            2
        } else if slow {
            1
        } else {
            0
        }
    });
    let (is_up, is_degraded, status_message) = classify_service_status(
        target.is_up,
        target.response_received,
        slow,
        service_status_code,
        telemetry.status_code.is_some(),
        telemetry.invalid_status_code,
        &monitor.response_codes,
    );
    target.message = combine_message(target.message, &status_message);
    CheckResult {
        monitor_id: monitor.id.clone(),
        checked_at_ms: Utc::now().timestamp_millis(),
        is_up,
        is_degraded,
        latency_ms: target.latency_ms,
        http_timings: target.http_timings,
        status_code: target.status_code,
        service_status_code: Some(service_status_code),
        metrics: telemetry.metrics,
        metric_units: telemetry.metric_units,
        message: target.message,
        tcp_response: target.tcp_response,
        tcp_response_truncated: target.tcp_response_truncated,
    }
}

#[derive(Default)]
struct ParsedKeyValues {
    status_code: Option<i64>,
    invalid_status_code: bool,
    metrics: BTreeMap<String, f64>,
    metric_units: BTreeMap<String, String>,
}

fn parse_key_values(body: &[u8]) -> ParsedKeyValues {
    let mut parsed = ParsedKeyValues::default();
    let text = String::from_utf8_lossy(body);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(separator) =
            line.find(|character: char| character.is_ascii_whitespace() || character == '=')
        else {
            continue;
        };
        let key = &line[..separator];
        let value = line[separator..].trim_start_matches(|character: char| {
            character.is_ascii_whitespace() || character == '='
        });
        let value = value.trim();
        if !valid_metric_key(key) || value.is_empty() {
            continue;
        }
        if key == "status.code" {
            match value.parse::<i64>() {
                Ok(code) => {
                    parsed.status_code = Some(code);
                    parsed.invalid_status_code = false;
                }
                Err(_) => {
                    parsed.status_code = None;
                    parsed.invalid_status_code = true;
                }
            }
        } else if let Some(group) = key.strip_suffix(".unit") {
            if unit_value_is_valid(value)
                && (parsed.metric_units.contains_key(group)
                    || parsed.metric_units.len() < MAX_METRIC_UNIT_COUNT)
            {
                parsed
                    .metric_units
                    .insert(group.to_owned(), value.to_owned());
            }
        } else if (parsed.metrics.contains_key(key) || parsed.metrics.len() < MAX_METRIC_COUNT)
            && let Ok(value) = value.parse::<f64>()
            && value.is_finite()
        {
            parsed.metrics.insert(key.to_owned(), value);
        }
    }
    parsed
}

fn unit_value_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_METRIC_UNIT_BYTES
        && !value.chars().any(char::is_control)
}

fn valid_metric_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_METRIC_KEY_BYTES
        && key.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
}

fn classify_service_status(
    response_up: bool,
    response_received: bool,
    slow: bool,
    code: i64,
    has_explicit_code: bool,
    invalid_code: bool,
    response_codes: &BTreeMap<i64, ResponseCodeStatus>,
) -> (bool, bool, String) {
    if !response_up || !response_received {
        return (
            false,
            false,
            if invalid_code {
                "Invalid status.code; no successful response was received".to_owned()
            } else if !response_received {
                "No response was received".to_owned()
            } else {
                String::new()
            },
        );
    }
    if invalid_code {
        return (false, false, "Invalid status.code".to_owned());
    }

    if has_explicit_code {
        match response_codes.get(&code) {
            Some(ResponseCodeStatus::Operational) if slow => (
                true,
                true,
                format!("status.code {code}; response exceeded slow_ms"),
            ),
            Some(ResponseCodeStatus::Operational) => (true, false, String::new()),
            Some(ResponseCodeStatus::Degraded) => (
                true,
                true,
                format!("status.code {code} reports degraded service"),
            ),
            Some(ResponseCodeStatus::Down) => (
                false,
                false,
                format!("status.code {code} reports service down"),
            ),
            None => (
                false,
                false,
                format!("status.code {code} is not configured"),
            ),
        }
    } else if slow {
        (true, true, "Response exceeded slow_ms".to_owned())
    } else {
        (true, false, String::new())
    }
}

fn combine_message(base: String, status: &str) -> String {
    if status.is_empty() {
        base
    } else if base.is_empty() {
        status.to_owned()
    } else {
        format!("{base} · {status}")
    }
}

async fn check_target(
    monitor: &MonitorConfig,
    http_probe_client: Option<&Result<HttpProbeClient, String>>,
) -> TargetResult {
    let timeout_duration = Duration::from_secs(monitor.timeout_seconds);
    match &monitor.target {
        MonitorTarget::Http {
            url,
            method,
            accepted_status_codes,
            headers,
        } => {
            let parsed_method =
                Method::from_bytes(method.to_ascii_uppercase().as_bytes()).unwrap_or(Method::GET);
            let http_client = match http_probe_client {
                Some(Ok(client)) => client,
                Some(Err(_)) => {
                    return TargetResult {
                        is_up: false,
                        latency_ms: None,
                        http_timings: None,
                        status_code: None,
                        message: "HTTP probe could not be initialized".to_owned(),
                        ..TargetResult::default()
                    };
                }
                None => {
                    return TargetResult {
                        is_up: false,
                        latency_ms: None,
                        http_timings: None,
                        status_code: None,
                        message: "HTTP probe is unavailable".to_owned(),
                        ..TargetResult::default()
                    };
                }
            };
            http_client.reset_timings();
            match http_client
                .send(url, parsed_method, headers, timeout_duration)
                .await
            {
                Err(error) => TargetResult {
                    is_up: false,
                    latency_ms: None,
                    http_timings: Some(http_client.finish_timings()),
                    status_code: None,
                    message: if error.contains("timed out") {
                        "HTTP request timed out".to_owned()
                    } else {
                        "HTTP request failed".to_owned()
                    },
                    ..TargetResult::default()
                },
                Ok(response) => {
                    let http_timings = http_client.finish_timings();
                    let elapsed_ms = http_timings.total_ms.unwrap_or_default();
                    let status_code = response.status_code;
                    let accepted = if accepted_status_codes.is_empty() {
                        (200..300).contains(&status_code)
                    } else {
                        accepted_status_codes.contains(&status_code)
                    };
                    TargetResult {
                        is_up: accepted,
                        response_received: true,
                        latency_ms: Some(elapsed_ms),
                        http_timings: Some(http_timings),
                        status_code: Some(status_code),
                        telemetry: Some(response.body),
                        message: if accepted {
                            format!("HTTP {status_code}")
                        } else {
                            format!("HTTP {status_code} was not an accepted status")
                        },
                        ..TargetResult::default()
                    }
                }
            }
        }
        MonitorTarget::Ping { host } => {
            let operation = async {
                let mut addresses = tokio::net::lookup_host((host.as_str(), 0))
                    .await
                    .map_err(|_| PingProbeFailure::DnsLookup)?;
                let address: SocketAddr = addresses.next().ok_or(PingProbeFailure::NoAddresses)?;
                let (_packet, duration) = surge_ping::ping(address.ip(), &[0; 8])
                    .await
                    .map_err(|_| PingProbeFailure::IcmpEcho)?;
                Ok::<_, PingProbeFailure>(duration)
            };
            match timeout(timeout_duration, operation).await {
                Err(_) => TargetResult {
                    is_up: false,
                    latency_ms: None,
                    http_timings: None,
                    status_code: None,
                    message: format!("Ping timed out after {}s", monitor.timeout_seconds),
                    ..TargetResult::default()
                },
                Ok(Err(error)) => TargetResult {
                    is_up: false,
                    latency_ms: None,
                    http_timings: None,
                    status_code: None,
                    message: error.public_message().to_owned(),
                    ..TargetResult::default()
                },
                Ok(Ok(duration)) => TargetResult {
                    is_up: true,
                    response_received: true,
                    latency_ms: Some(duration.as_secs_f64() * 1000.0),
                    http_timings: None,
                    status_code: None,
                    message: format!(
                        "ICMP reply received ({:.2} ms)",
                        duration.as_secs_f64() * 1000.0
                    ),
                    ..TargetResult::default()
                },
            }
        }
        MonitorTarget::Tcp {
            host,
            port,
            send_message,
            response_terminator,
            max_response_bytes,
        } => {
            let started = Instant::now();
            let mut connected = false;
            let mut response_bytes = Vec::new();
            let mut response_truncated = false;
            let operation = async {
                let addresses = tokio::net::lookup_host((host.as_str(), *port))
                    .await
                    .map_err(|_| TcpProbeFailure::DnsLookup)?;
                let mut stream = None;
                let mut found_address = false;
                for address in addresses {
                    found_address = true;
                    match tokio::net::TcpStream::connect(address).await {
                        Ok(connected_stream) => {
                            stream = Some(connected_stream);
                            break;
                        }
                        Err(_) => {}
                    }
                }
                let mut stream = stream.ok_or(if found_address {
                    TcpProbeFailure::Connect
                } else {
                    TcpProbeFailure::NoAddresses
                })?;
                connected = true;

                if let Some(message) = send_message {
                    stream
                        .write_all(message.as_bytes())
                        .await
                        .map_err(|_| TcpProbeFailure::Send)?;
                    response_truncated = read_tcp_response(
                        &mut stream,
                        response_terminator.as_deref(),
                        *max_response_bytes,
                        &mut response_bytes,
                    )
                    .await?;
                    if let Some(terminator) = response_terminator
                        && !response_bytes.ends_with(terminator.as_bytes())
                    {
                        return Err(TcpProbeFailure::ResponseTerminator);
                    }
                }

                Ok::<(), TcpProbeFailure>(())
            };
            match timeout(timeout_duration, operation).await {
                Err(_) => TargetResult {
                    is_up: false,
                    response_received: !response_bytes.is_empty(),
                    latency_ms: connected.then(|| started.elapsed().as_secs_f64() * 1000.0),
                    http_timings: None,
                    status_code: None,
                    telemetry: (!response_bytes.is_empty()).then(|| response_bytes.clone()),
                    message: if send_message.is_some() {
                        format!("TCP exchange timed out after {}s", monitor.timeout_seconds)
                    } else {
                        format!(
                            "TCP connection timed out after {}s",
                            monitor.timeout_seconds
                        )
                    },
                    tcp_response: (!response_bytes.is_empty()).then(|| response_bytes.clone()),
                    tcp_response_truncated: response_truncated
                        || (connected && !response_bytes.is_empty()),
                },
                Ok(Err(error)) => TargetResult {
                    is_up: false,
                    response_received: !response_bytes.is_empty(),
                    latency_ms: connected.then(|| started.elapsed().as_secs_f64() * 1000.0),
                    http_timings: None,
                    status_code: None,
                    telemetry: (!response_bytes.is_empty()).then(|| response_bytes.clone()),
                    message: error.public_message().to_owned(),
                    tcp_response: (!response_bytes.is_empty()).then(|| response_bytes.clone()),
                    tcp_response_truncated: response_truncated
                        || (connected && !response_bytes.is_empty()),
                },
                Ok(Ok(())) => {
                    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
                    let message = if send_message.is_some() {
                        if response_truncated {
                            format!(
                                "TCP exchange succeeded; response capture was truncated at {max_response_bytes} bytes"
                            )
                        } else {
                            "TCP exchange succeeded".to_owned()
                        }
                    } else {
                        "TCP connection succeeded".to_owned()
                    };
                    TargetResult {
                        is_up: true,
                        response_received: send_message.is_none() || !response_bytes.is_empty(),
                        latency_ms: Some(latency_ms),
                        http_timings: None,
                        status_code: None,
                        telemetry: (!response_bytes.is_empty()).then(|| response_bytes.clone()),
                        message,
                        tcp_response: (!response_bytes.is_empty()).then(|| response_bytes.clone()),
                        tcp_response_truncated: response_truncated,
                    }
                }
            }
        }
    }
}

async fn read_tcp_response(
    stream: &mut tokio::net::TcpStream,
    terminator: Option<&str>,
    max_response_bytes: usize,
    response: &mut Vec<u8>,
) -> Result<bool, TcpProbeFailure> {
    let mut buffer = [0; 1024];
    loop {
        let remaining = max_response_bytes.saturating_sub(response.len());
        if remaining == 0 {
            return Ok(true);
        }
        let read_len = remaining.min(buffer.len());
        let count = stream
            .read(&mut buffer[..read_len])
            .await
            .map_err(|_| TcpProbeFailure::Read)?;
        if count == 0 {
            return Ok(false);
        }
        response.extend_from_slice(&buffer[..count]);

        if let Some(terminator) = terminator {
            let terminator = terminator.as_bytes();
            if let Some(end) = response
                .windows(terminator.len())
                .position(|window| window == terminator)
                .map(|position| position + terminator.len())
            {
                response.truncate(end);
                return Ok(false);
            }
        }
    }
}
