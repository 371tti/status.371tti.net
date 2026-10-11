#[cfg(target_os = "linux")]
mod linux {
    use std::{
        collections::BTreeMap,
        error::Error,
        fmt::Write as FmtWrite,
        fs,
        io::{self, Read, Write as IoWrite},
        net::{Shutdown, SocketAddr, TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    const DEFAULT_BIND: &str = "127.0.0.1";
    const DEFAULT_PORT: u16 = 9791;
    const MAX_REQUEST_BYTES: usize = 256;
    const MAX_CONNECTIONS: usize = 64;
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

    struct State {
        token: Option<String>,
        started_at: Instant,
        options: MetricOptions,
        cpu_metrics: Option<Arc<Mutex<Option<CpuMetrics>>>>,
        network_previous: Mutex<Option<NetworkSnapshot>>,
    }

    #[derive(Clone, Copy)]
    struct MetricOptions {
        cpu: bool,
        memory: bool,
        load: bool,
        network: bool,
    }

    struct ConnectionPermit(Arc<AtomicUsize>);

    impl Drop for ConnectionPermit {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[derive(Clone, Copy)]
    struct CpuTimes {
        total: u64,
        idle: u64,
        iowait: u64,
    }

    struct CpuSnapshot {
        aggregate: CpuTimes,
        cores: BTreeMap<usize, CpuTimes>,
    }

    #[derive(Clone, Copy)]
    struct CpuPercentages {
        usage: f64,
        iowait: f64,
    }

    #[derive(Clone)]
    struct CpuMetrics {
        aggregate: CpuPercentages,
        cores: BTreeMap<usize, CpuPercentages>,
    }

    struct Memory {
        total_bytes: u64,
        available_bytes: u64,
    }

    #[derive(Clone, Copy, Default)]
    struct NetworkCounters {
        rx_bytes: u64,
        rx_packets: u64,
        rx_errors: u64,
        rx_drops: u64,
        tx_bytes: u64,
        tx_packets: u64,
        tx_errors: u64,
        tx_drops: u64,
    }

    struct NetworkSnapshot {
        sampled_at: Instant,
        interfaces: BTreeMap<String, NetworkCounters>,
    }

    #[derive(Clone, Copy)]
    struct NetworkRates {
        rx_bytes_per_second: f64,
        tx_bytes_per_second: f64,
        rx_packets_per_second: f64,
        tx_packets_per_second: f64,
        rx_errors_per_second: f64,
        tx_errors_per_second: f64,
        rx_drops_per_second: f64,
        tx_drops_per_second: f64,
    }

    struct NetworkSample {
        interface_count: usize,
        interfaces: BTreeMap<String, NetworkRates>,
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let bind = std::env::var("WS371_AGENT_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_owned());
        let port = std::env::var("WS371_AGENT_PORT")
            .unwrap_or_else(|_| DEFAULT_PORT.to_string())
            .parse::<u16>()?;
        if port == 0 {
            return Err("WS371_AGENT_PORT must be between 1 and 65535".into());
        }
        let address = socket_address(&bind, port)?;
        let token = std::env::var("WS371_AGENT_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());
        if token
            .as_deref()
            .is_some_and(|value| value.len() > 128 || value.contains(['\r', '\n']))
        {
            return Err(
                "WS371_AGENT_TOKEN must be at most 128 bytes and contain no newlines".into(),
            );
        }

        let options = MetricOptions {
            cpu: read_env_flag("WS371_AGENT_ENABLE_CPU", true)?,
            memory: read_env_flag("WS371_AGENT_ENABLE_MEMORY", true)?,
            load: read_env_flag("WS371_AGENT_ENABLE_LOAD", true)?,
            network: read_env_flag("WS371_AGENT_ENABLE_NETWORK", true)?,
        };
        let cpu_metrics = if options.cpu {
            let metrics = Arc::new(Mutex::new(None));
            let cpu_sample_ready = start_cpu_sampler(Arc::clone(&metrics));
            let _ = cpu_sample_ready.recv_timeout(Duration::from_secs(3));
            Some(metrics)
        } else {
            None
        };
        let network_previous = if options.network {
            read_network_counters()
                .ok()
                .map(|interfaces| NetworkSnapshot {
                    sampled_at: Instant::now(),
                    interfaces,
                })
        } else {
            None
        };
        let state = Arc::new(State {
            token,
            started_at: Instant::now(),
            options,
            cpu_metrics,
            network_previous: Mutex::new(network_previous),
        });
        let active_connections = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind(address)?;
        eprintln!("wk-status-371tti-net-agent listening on {address}");

        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("TCP accept failed: {error}");
                    continue;
                }
            };

            if !acquire_connection(&active_connections) {
                drop(stream);
                continue;
            }

            let state = Arc::clone(&state);
            let worker_connections = Arc::clone(&active_connections);
            let worker = thread::Builder::new()
                .name("metrics-client".to_owned())
                .spawn(move || {
                    let _permit = ConnectionPermit(worker_connections);
                    let _ = handle_connection(stream, &state);
                });
            if let Err(error) = worker {
                eprintln!("could not start TCP client worker: {error}");
                active_connections.fetch_sub(1, Ordering::Relaxed);
            }
        }

        Ok(())
    }

    fn read_env_flag(name: &str, default: bool) -> Result<bool, Box<dyn Error>> {
        let Some(value) = std::env::var_os(name) else {
            return Ok(default);
        };
        match value.to_string_lossy().trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(format!("{name} must be set to 1 or 0").into()),
        }
    }

    fn socket_address(bind: &str, port: u16) -> Result<SocketAddr, Box<dyn Error>> {
        let address = if bind.contains(':') {
            format!("[{bind}]:{port}")
        } else {
            format!("{bind}:{port}")
        };
        Ok(address.parse()?)
    }

    fn acquire_connection(active: &AtomicUsize) -> bool {
        let mut current = active.load(Ordering::Relaxed);
        loop {
            if current >= MAX_CONNECTIONS {
                return false;
            }
            match active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }

    fn handle_connection(mut stream: TcpStream, state: &State) -> io::Result<()> {
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;

        let request = read_request_line(&mut stream)?;
        let expected = match state.token.as_deref() {
            Some(token) => format!("METRICS {token}"),
            None => "METRICS".to_owned(),
        };
        if !constant_time_eq(request.as_bytes(), expected.as_bytes()) {
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }

        let response = collect_metrics(state);
        stream.write_all(response.as_bytes())?;
        stream.shutdown(Shutdown::Write)
    }

    fn read_request_line(stream: &mut TcpStream) -> io::Result<String> {
        let mut request = Vec::with_capacity(48);
        let mut byte = [0_u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) if request.is_empty() => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "request ended before a command was received",
                    ));
                }
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) if request.len() < MAX_REQUEST_BYTES => request.push(byte[0]),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "request line is too long",
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        if request.last() == Some(&b'\r') {
            request.pop();
        }
        String::from_utf8(request)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request is not UTF-8"))
    }

    fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
        let mut difference = left.len() ^ right.len();
        for index in 0..left.len().max(right.len()) {
            let a = left.get(index).copied().unwrap_or(0);
            let b = right.get(index).copied().unwrap_or(0);
            difference |= usize::from(a ^ b);
        }
        difference == 0
    }

    fn collect_metrics(state: &State) -> String {
        let cpu_metrics = state.cpu_metrics.as_ref().and_then(|metrics| {
            metrics
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        });
        let memory = state.options.memory.then(read_memory);
        let uptime = read_host_uptime();
        let load = state.options.load.then(read_load_average);
        let network = state.options.network.then(|| sample_network(state));
        let mut status_code = 0;
        if (state.options.cpu && cpu_metrics.is_none())
            || memory.as_ref().is_some_and(Result::is_err)
            || uptime.is_err()
            || load.as_ref().is_some_and(Result::is_err)
            || network.as_ref().is_some_and(Result::is_err)
        {
            status_code = 1;
        }

        let mut output = String::new();
        let _ = writeln!(output, "status.code {status_code}");
        if let Some(cpu_metrics) = cpu_metrics {
            let _ = writeln!(output, "cpu.cores.unit cores");
            let _ = writeln!(output, "cpu.cores.count {}", cpu_metrics.cores.len());
            let _ = writeln!(output, "cpu.percent.unit %");
            let _ = writeln!(
                output,
                "cpu.percent.usage {:.2}",
                cpu_metrics.aggregate.usage
            );
            let _ = writeln!(
                output,
                "cpu.percent.iowait {:.2}",
                cpu_metrics.aggregate.iowait
            );
            for (core, percentages) in cpu_metrics.cores {
                let _ = writeln!(output, "cpu.core.{core}.percent.unit %");
                let _ = writeln!(
                    output,
                    "cpu.core.{core}.percent.usage {:.2}",
                    percentages.usage
                );
                let _ = writeln!(
                    output,
                    "cpu.core.{core}.percent.iowait {:.2}",
                    percentages.iowait
                );
            }
        }
        if let Some(Ok(memory)) = memory.as_ref() {
            let used_bytes = memory.total_bytes.saturating_sub(memory.available_bytes);
            let usage_percent = if memory.total_bytes == 0 {
                0.0
            } else {
                used_bytes as f64 * 100.0 / memory.total_bytes as f64
            };
            let _ = writeln!(output, "memory.bytes.unit B");
            let _ = writeln!(output, "memory.bytes.used {used_bytes}");
            let _ = writeln!(output, "memory.bytes.available {}", memory.available_bytes);
            let _ = writeln!(output, "memory.bytes.total {}", memory.total_bytes);
            let _ = writeln!(output, "memory.percent.unit %");
            let _ = writeln!(output, "memory.percent.usage {usage_percent:.2}");
        }
        if let Some(Ok(load)) = load.as_ref() {
            let _ = writeln!(output, "system.load.unit tasks");
            let _ = writeln!(output, "system.load.1 {:.2}", load[0]);
            let _ = writeln!(output, "system.load.5 {:.2}", load[1]);
            let _ = writeln!(output, "system.load.15 {:.2}", load[2]);
        }
        if let Some(Ok(Some(network))) = network.as_ref() {
            let _ = writeln!(output, "network.interfaces.unit interfaces");
            let _ = writeln!(
                output,
                "network.interfaces.count {}",
                network.interface_count
            );
            for (interface, rates) in &network.interfaces {
                let interface = metric_segment(interface);
                let _ = writeln!(output, "network.{interface}.bytes.unit B/s");
                let _ = writeln!(
                    output,
                    "network.{interface}.bytes.rx {:.2}",
                    rates.rx_bytes_per_second
                );
                let _ = writeln!(
                    output,
                    "network.{interface}.bytes.tx {:.2}",
                    rates.tx_bytes_per_second
                );
                let _ = writeln!(output, "network.{interface}.packets.unit packets/s");
                let _ = writeln!(
                    output,
                    "network.{interface}.packets.rx {:.2}",
                    rates.rx_packets_per_second
                );
                let _ = writeln!(
                    output,
                    "network.{interface}.packets.tx {:.2}",
                    rates.tx_packets_per_second
                );
                let _ = writeln!(output, "network.{interface}.errors.unit errors/s");
                let _ = writeln!(
                    output,
                    "network.{interface}.errors.rx {:.2}",
                    rates.rx_errors_per_second
                );
                let _ = writeln!(
                    output,
                    "network.{interface}.errors.tx {:.2}",
                    rates.tx_errors_per_second
                );
                let _ = writeln!(output, "network.{interface}.drops.unit drops/s");
                let _ = writeln!(
                    output,
                    "network.{interface}.drops.rx {:.2}",
                    rates.rx_drops_per_second
                );
                let _ = writeln!(
                    output,
                    "network.{interface}.drops.tx {:.2}",
                    rates.tx_drops_per_second
                );
            }
        }
        if let Ok(uptime) = uptime {
            let _ = writeln!(output, "system.unit s");
            let _ = writeln!(output, "system.uptime_seconds {uptime:.2}");
        }
        let _ = writeln!(output, "agent.seconds.unit s");
        let _ = writeln!(
            output,
            "agent.seconds.uptime {:.2}",
            state.started_at.elapsed().as_secs_f64()
        );
        output
    }

    fn sample_network(state: &State) -> io::Result<Option<NetworkSample>> {
        let interfaces = read_network_counters()?;
        let sampled_at = Instant::now();
        let mut previous = state
            .network_previous
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let rates = previous.as_ref().map(|previous| {
            let elapsed_seconds = sampled_at
                .duration_since(previous.sampled_at)
                .as_secs_f64()
                .max(0.001);
            interfaces
                .iter()
                .filter_map(|(name, current)| {
                    let previous = previous.interfaces.get(name)?;
                    Some((
                        name.clone(),
                        NetworkRates {
                            rx_bytes_per_second: counter_rate(
                                current.rx_bytes,
                                previous.rx_bytes,
                                elapsed_seconds,
                            ),
                            tx_bytes_per_second: counter_rate(
                                current.tx_bytes,
                                previous.tx_bytes,
                                elapsed_seconds,
                            ),
                            rx_packets_per_second: counter_rate(
                                current.rx_packets,
                                previous.rx_packets,
                                elapsed_seconds,
                            ),
                            tx_packets_per_second: counter_rate(
                                current.tx_packets,
                                previous.tx_packets,
                                elapsed_seconds,
                            ),
                            rx_errors_per_second: counter_rate(
                                current.rx_errors,
                                previous.rx_errors,
                                elapsed_seconds,
                            ),
                            tx_errors_per_second: counter_rate(
                                current.tx_errors,
                                previous.tx_errors,
                                elapsed_seconds,
                            ),
                            rx_drops_per_second: counter_rate(
                                current.rx_drops,
                                previous.rx_drops,
                                elapsed_seconds,
                            ),
                            tx_drops_per_second: counter_rate(
                                current.tx_drops,
                                previous.tx_drops,
                                elapsed_seconds,
                            ),
                        },
                    ))
                })
                .collect::<BTreeMap<_, _>>()
        });
        let interface_count = interfaces.len();
        *previous = Some(NetworkSnapshot {
            sampled_at,
            interfaces,
        });
        Ok(rates.map(|interfaces| NetworkSample {
            interface_count,
            interfaces,
        }))
    }

    fn counter_rate(current: u64, previous: u64, elapsed_seconds: f64) -> f64 {
        current.saturating_sub(previous) as f64 / elapsed_seconds
    }

    fn metric_segment(name: &str) -> String {
        let segment = name
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                    byte as char
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if segment.is_empty() {
            "unknown".to_owned()
        } else {
            segment
        }
    }

    fn read_network_counters() -> io::Result<BTreeMap<String, NetworkCounters>> {
        let contents = fs::read_to_string("/proc/net/dev")?;
        let mut interfaces = BTreeMap::new();
        for line in contents.lines().skip(2) {
            let Some((name, counters)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let values = counters
                .split_ascii_whitespace()
                .map(str::parse::<u64>)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if values.len() < 16 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete network counters",
                ));
            }
            interfaces.insert(
                name.to_owned(),
                NetworkCounters {
                    rx_bytes: values[0],
                    rx_packets: values[1],
                    rx_errors: values[2],
                    rx_drops: values[3],
                    tx_bytes: values[8],
                    tx_packets: values[9],
                    tx_errors: values[10],
                    tx_drops: values[11],
                },
            );
        }
        Ok(interfaces)
    }

    fn start_cpu_sampler(metrics: Arc<Mutex<Option<CpuMetrics>>>) -> mpsc::Receiver<()> {
        let (ready_sender, ready_receiver) = mpsc::channel();
        thread::Builder::new()
            .name("cpu-sampler".to_owned())
            .spawn(move || {
                let mut previous = read_cpu_times().ok();
                let mut ready_sender = Some(ready_sender);
                loop {
                    thread::sleep(Duration::from_secs(1));
                    let Ok(current) = read_cpu_times() else {
                        continue;
                    };
                    if let Some(previous) = previous.as_ref() {
                        if let Some(aggregate) =
                            calculate_cpu_percentages(previous.aggregate, current.aggregate)
                        {
                            let cores = current
                                .cores
                                .iter()
                                .filter_map(|(core, counters)| {
                                    let previous = previous.cores.get(core)?;
                                    Some((*core, calculate_cpu_percentages(*previous, *counters)?))
                                })
                                .collect();
                            *metrics.lock().unwrap_or_else(|error| error.into_inner()) =
                                Some(CpuMetrics { aggregate, cores });
                            if let Some(sender) = ready_sender.take() {
                                let _ = sender.send(());
                            }
                        }
                    }
                    previous = Some(current);
                }
            })
            .expect("failed to start CPU sampler");
        ready_receiver
    }

    fn calculate_cpu_percentages(previous: CpuTimes, current: CpuTimes) -> Option<CpuPercentages> {
        let total_delta = current.total.saturating_sub(previous.total);
        if total_delta == 0 {
            return None;
        }
        let idle_delta = current.idle.saturating_sub(previous.idle);
        let iowait_delta = current.iowait.saturating_sub(previous.iowait);
        let busy_delta = total_delta
            .saturating_sub(idle_delta)
            .saturating_sub(iowait_delta);
        Some(CpuPercentages {
            usage: (busy_delta as f64 * 100.0 / total_delta as f64).clamp(0.0, 100.0),
            iowait: (iowait_delta as f64 * 100.0 / total_delta as f64).clamp(0.0, 100.0),
        })
    }

    fn read_cpu_times() -> io::Result<CpuSnapshot> {
        let contents = fs::read_to_string("/proc/stat")?;
        let mut aggregate = None;
        let mut cores = BTreeMap::new();
        for line in contents.lines() {
            let mut fields = line.split_ascii_whitespace();
            let Some(name) = fields.next() else {
                continue;
            };
            let core = if name == "cpu" {
                None
            } else if let Some(number) = name.strip_prefix("cpu") {
                if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
                    continue;
                }
                Some(
                    number
                        .parse::<usize>()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                )
            } else {
                continue;
            };
            let values = fields
                .map(str::parse::<u64>)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if values.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete CPU counters",
                ));
            }
            let total = values
                .iter()
                .copied()
                .take(8)
                .fold(0_u64, u64::saturating_add);
            let times = CpuTimes {
                total,
                idle: values[3],
                iowait: values.get(4).copied().unwrap_or_default(),
            };
            if let Some(core) = core {
                cores.insert(core, times);
            } else {
                aggregate = Some(times);
            }
        }
        let aggregate = aggregate
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing CPU counters"))?;
        Ok(CpuSnapshot { aggregate, cores })
    }

    fn read_memory() -> io::Result<Memory> {
        let contents = fs::read_to_string("/proc/meminfo")?;
        let mut total_kib = None;
        let mut available_kib = None;
        let mut fallback_available_kib = 0_u64;
        for line in contents.lines() {
            let Some((key, rest)) = line.split_once(':') else {
                continue;
            };
            let Some(value) = rest.split_ascii_whitespace().next() else {
                continue;
            };
            let Ok(value) = value.parse::<u64>() else {
                continue;
            };
            match key {
                "MemTotal" => total_kib = Some(value),
                "MemAvailable" => available_kib = Some(value),
                "MemFree" | "Buffers" | "Cached" => {
                    fallback_available_kib = fallback_available_kib.saturating_add(value);
                }
                _ => {}
            }
        }
        let total_kib = total_kib
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing MemTotal"))?;
        let available_kib = available_kib.unwrap_or(fallback_available_kib);
        Ok(Memory {
            total_bytes: total_kib.saturating_mul(1024),
            available_bytes: available_kib.saturating_mul(1024),
        })
    }

    fn read_host_uptime() -> io::Result<f64> {
        let contents = fs::read_to_string("/proc/uptime")?;
        contents
            .split_ascii_whitespace()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing system uptime"))?
            .parse::<f64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn read_load_average() -> io::Result<[f64; 3]> {
        let contents = fs::read_to_string("/proc/loadavg")?;
        let mut values = contents
            .split_ascii_whitespace()
            .take(3)
            .map(str::parse::<f64>);
        let mut load = [0.0; 3];
        for value in &mut load {
            *value = values
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing load value"))?
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
        Ok(load)
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = linux::run() {
        eprintln!("wk-status-371tti-net-agent: {error}");
        std::process::exit(1);
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("wk-status-371tti-net-agent is supported on Linux only");
        std::process::exit(1);
    }
}
