#[cfg(target_os = "linux")]
mod linux {
    use std::{
        collections::{BTreeMap, BTreeSet},
        error::Error,
        fmt::Write as FmtWrite,
        fs,
        io::{self, Read, Write as IoWrite},
        net::{Shutdown, SocketAddr, TcpListener, TcpStream},
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
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
        environment: RuntimeEnvironment,
        cgroups: Option<CgroupResources>,
        lxcfs_proc: LxcfsProcFiles,
        status_thresholds: StatusThresholds,
        cpu_metrics: Option<Arc<Mutex<Option<CpuMetrics>>>>,
        cpu_sampler_error: Option<Arc<AtomicBool>>,
        network_previous: Mutex<Option<NetworkSnapshot>>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RuntimeEnvironment {
        Lxc,
        Container,
        VirtualMachine,
        Host,
    }

    impl RuntimeEnvironment {
        fn is_container(self) -> bool {
            matches!(self, Self::Lxc | Self::Container)
        }

        fn label(self) -> &'static str {
            match self {
                Self::Lxc => "LXC container",
                Self::Container => "container",
                Self::VirtualMachine => "virtual machine guest",
                Self::Host => "host",
            }
        }
    }

    #[derive(Clone, Copy, Default)]
    struct LxcfsProcFiles {
        stat: bool,
        meminfo: bool,
    }

    struct CgroupResources {
        cpu: Option<CgroupCpuPath>,
        memory: Option<CgroupMemoryPath>,
    }

    #[derive(Clone)]
    enum CgroupCpuPath {
        V2(PathBuf),
        V1 {
            cpu: Option<PathBuf>,
            cpuacct: PathBuf,
            cpuset: Option<PathBuf>,
        },
    }

    #[derive(Clone)]
    enum CgroupMemoryPath {
        V2(PathBuf),
        V1(PathBuf),
    }

    struct CgroupMountInfo {
        kind: String,
        root: PathBuf,
        mountpoint: PathBuf,
        controllers: BTreeSet<String>,
    }

    #[derive(Clone, Copy)]
    struct MetricOptions {
        cpu: bool,
        memory: bool,
        load: bool,
        network: bool,
    }

    struct StatusThresholds {
        cpu_percent: Option<f64>,
        memory_percent: Option<f64>,
        iowait_percent: Option<f64>,
        nic_error_drop_percent: Option<f64>,
        network_min_packets: u64,
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
        iowait: Option<f64>,
    }

    #[derive(Clone)]
    struct CpuMetrics {
        aggregate: CpuPercentages,
        cores: BTreeMap<usize, CpuPercentages>,
    }

    enum CpuSamplerSource {
        Proc,
        Cgroup(CgroupCpuPath),
    }

    enum CpuSample {
        Proc(CpuSnapshot),
        Cgroup {
            usage_ns: u64,
            capacity_cores: f64,
            cores: Option<BTreeMap<usize, u64>>,
        },
    }

    struct TimedCpuSample {
        sampled_at: Instant,
        sample: CpuSample,
    }

    struct Memory {
        used_bytes: u64,
        total_bytes: Option<u64>,
        available_bytes: Option<u64>,
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
        rx_error_drop_percent: f64,
        rx_observed_packets: u64,
        tx_packets_per_second: f64,
        tx_error_drop_percent: f64,
        tx_observed_packets: u64,
        rx_errors_per_second: f64,
        tx_errors_per_second: f64,
        rx_drops_per_second: f64,
        tx_drops_per_second: f64,
    }

    struct NetworkSample {
        interface_count: usize,
        interfaces: BTreeMap<String, NetworkRates>,
    }

    impl NetworkSample {
        fn maximum_error_drop_percent(&self, minimum_packets: u64) -> Option<f64> {
            let mut maximum = None::<f64>;
            for rates in self.interfaces.values() {
                for (observed_packets, error_drop_percent) in [
                    (rates.rx_observed_packets, rates.rx_error_drop_percent),
                    (rates.tx_observed_packets, rates.tx_error_drop_percent),
                ] {
                    if observed_packets >= minimum_packets {
                        maximum = Some(
                            maximum
                                .map_or(error_drop_percent, |value| value.max(error_drop_percent)),
                        );
                    }
                }
            }
            maximum
        }
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
        let status_thresholds = StatusThresholds {
            cpu_percent: read_percent_threshold("WS371_AGENT_WARN_CPU_PERCENT", 90.0)?,
            memory_percent: read_percent_threshold("WS371_AGENT_WARN_MEMORY_PERCENT", 90.0)?,
            iowait_percent: read_percent_threshold("WS371_AGENT_WARN_IOWAIT_PERCENT", 20.0)?,
            nic_error_drop_percent: read_percent_threshold(
                "WS371_AGENT_WARN_NIC_ERROR_DROP_PERCENT",
                1.0,
            )?,
            network_min_packets: read_positive_u64("WS371_AGENT_STATUS_NETWORK_MIN_PACKETS", 100)?,
        };
        let lxcfs_proc = detect_lxcfs_proc_files();
        let environment = detect_runtime_environment(lxcfs_proc);
        let cgroups = environment
            .is_container()
            .then(discover_cgroup_resources)
            .flatten();
        eprintln!("detected {}", environment.label());
        if environment.is_container() && cgroups.is_none() {
            eprintln!(
                "container cgroups could not be resolved; host-wide proc values will be omitted where they cannot be scoped safely"
            );
        }
        let cpu_source = if !environment.is_container() || lxcfs_proc.stat {
            Some(CpuSamplerSource::Proc)
        } else {
            cgroups
                .as_ref()
                .and_then(|resources| resources.cpu.clone())
                .map(CpuSamplerSource::Cgroup)
        };
        if options.cpu && cpu_source.is_none() {
            eprintln!("CPU metrics are unavailable: no container-scoped CPU source was found");
        }
        let mut cpu_sampler_error = None;
        let cpu_metrics = if options.cpu {
            if let Some(cpu_source) = cpu_source {
                let metrics = Arc::new(Mutex::new(None));
                let sample_error = Arc::new(AtomicBool::new(false));
                let cpu_sample_ready =
                    start_cpu_sampler(Arc::clone(&metrics), Arc::clone(&sample_error), cpu_source);
                let _ = cpu_sample_ready.recv_timeout(Duration::from_secs(3));
                cpu_sampler_error = Some(sample_error);
                Some(metrics)
            } else {
                None
            }
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
            environment,
            cgroups,
            lxcfs_proc,
            status_thresholds,
            cpu_metrics,
            cpu_sampler_error,
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

    fn read_percent_threshold(name: &str, default: f64) -> Result<Option<f64>, Box<dyn Error>> {
        let Some(value) = std::env::var_os(name) else {
            return Ok(Some(default));
        };
        let value = value.to_string_lossy();
        if value.trim().eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let threshold = value.parse::<f64>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{name} must be a percentage greater than 0 and at most 100, or 'off': {error}"
                ),
            )
        })?;
        if !threshold.is_finite() || !(threshold > 0.0 && threshold <= 100.0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} must be a percentage greater than 0 and at most 100, or 'off'"),
            )
            .into());
        }
        Ok(Some(threshold))
    }

    fn read_positive_u64(name: &str, default: u64) -> Result<u64, Box<dyn Error>> {
        let Some(value) = std::env::var_os(name) else {
            return Ok(default);
        };
        let value = value.to_string_lossy();
        let count = value.parse::<u64>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} must be a positive integer: {error}"),
            )
        })?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} must be a positive integer"),
            )
            .into());
        }
        Ok(count)
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
        let memory = state.options.memory.then(|| {
            if state.environment.is_container() {
                if let Some(path) = state
                    .cgroups
                    .as_ref()
                    .and_then(|resources| resources.memory.as_ref())
                {
                    return read_cgroup_memory(path);
                }
                if !state.lxcfs_proc.meminfo {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "container-scoped memory data is unavailable",
                    ));
                }
            }
            read_memory()
        });
        let uptime = (!state.environment.is_container()).then(read_host_uptime);
        let load =
            (state.options.load && !state.environment.is_container()).then(read_load_average);
        let network = state.options.network.then(|| sample_network(state));
        let mut status_code = 0;
        if (state.options.cpu && cpu_metrics.is_none())
            || state
                .cpu_sampler_error
                .as_ref()
                .is_some_and(|failed| failed.load(Ordering::Relaxed))
            || memory.as_ref().is_some_and(Result::is_err)
            || uptime.as_ref().is_some_and(Result::is_err)
            || load.as_ref().is_some_and(Result::is_err)
            || network.as_ref().is_some_and(Result::is_err)
        {
            status_code = 1;
        }
        if status_code == 0
            && status_threshold_exceeded(
                &state.status_thresholds,
                cpu_metrics.as_ref(),
                memory.as_ref(),
                network.as_ref(),
            )
        {
            status_code = 1;
        }

        let memory_limit = if state.options.memory {
            memory
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(|memory| memory.total_bytes)
                .or_else(|| memory_capacity_bytes(state))
        } else {
            None
        };

        let mut output = String::new();
        let _ = writeln!(output, "status.code {status_code}");
        if state.options.cpu {
            let _ = writeln!(output, "cpu.percent.unit %");
            let _ = writeln!(output, "cpu.percent.limit 100");
            let _ = writeln!(output, "cpu.iowait.unit %");
            let _ = writeln!(output, "cpu.iowait.limit 100");
            if let Some(cpu_metrics) = cpu_metrics {
                if !cpu_metrics.cores.is_empty() {
                    let _ = writeln!(output, "cpu.cores.unit cores");
                    let _ = writeln!(output, "cpu.cores.count {}", cpu_metrics.cores.len());
                }
                let _ = writeln!(output, "cpu.percent.all {:.2}", cpu_metrics.aggregate.usage);
                if let Some(iowait) = cpu_metrics.aggregate.iowait {
                    let _ = writeln!(output, "cpu.iowait.all {iowait:.2}");
                }
                for (core, percentages) in cpu_metrics.cores {
                    let _ = writeln!(output, "cpu.percent.{core} {:.2}", percentages.usage);
                    if let Some(iowait) = percentages.iowait {
                        let _ = writeln!(output, "cpu.iowait.{core} {iowait:.2}");
                    }
                }
            }
        }
        if state.options.memory {
            let _ = writeln!(output, "memory.bytes.unit B");
            let _ = writeln!(output, "memory.percent.unit %");
            let _ = writeln!(output, "memory.percent.limit 100");
            if let Some(total_bytes) = memory_limit {
                let _ = writeln!(output, "memory.bytes.total {total_bytes}");
            }
        }
        if state.options.network {
            for group in [
                "network.error_drop.rx.percent",
                "network.error_drop.tx.percent",
            ] {
                let _ = writeln!(output, "{group}.unit %");
                let _ = writeln!(output, "{group}.limit 100");
            }
        }
        if let Some(Ok(memory)) = memory.as_ref() {
            let _ = writeln!(output, "memory.bytes.used {}", memory.used_bytes);
            if let Some(available_bytes) = memory.available_bytes {
                let _ = writeln!(output, "memory.bytes.available {available_bytes}");
            }
            if let Some(total_bytes) = memory.total_bytes.or(memory_limit) {
                let usage_percent = if total_bytes == 0 {
                    0.0
                } else {
                    (memory.used_bytes as f64 * 100.0 / total_bytes as f64).clamp(0.0, 100.0)
                };
                let _ = writeln!(output, "memory.percent.usage {usage_percent:.2}");
            }
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
            for (group, unit) in [
                ("network.bytes.rx", "B/s"),
                ("network.bytes.tx", "B/s"),
                ("network.packets.rx", "packets/s"),
                ("network.packets.tx", "packets/s"),
                ("network.errors.rx", "errors/s"),
                ("network.errors.tx", "errors/s"),
                ("network.drops.rx", "drops/s"),
                ("network.drops.tx", "drops/s"),
            ] {
                let _ = writeln!(output, "{group}.unit {unit}");
            }
            for (interface, rates) in &network.interfaces {
                let interface = metric_segment(interface);
                let _ = writeln!(
                    output,
                    "network.bytes.rx.{interface} {:.2}",
                    rates.rx_bytes_per_second
                );
                let _ = writeln!(
                    output,
                    "network.bytes.tx.{interface} {:.2}",
                    rates.tx_bytes_per_second
                );
                let _ = writeln!(
                    output,
                    "network.packets.rx.{interface} {:.2}",
                    rates.rx_packets_per_second
                );
                let _ = writeln!(
                    output,
                    "network.packets.tx.{interface} {:.2}",
                    rates.tx_packets_per_second
                );
                let _ = writeln!(
                    output,
                    "network.errors.rx.{interface} {:.2}",
                    rates.rx_errors_per_second
                );
                let _ = writeln!(
                    output,
                    "network.errors.tx.{interface} {:.2}",
                    rates.tx_errors_per_second
                );
                let _ = writeln!(
                    output,
                    "network.drops.rx.{interface} {:.2}",
                    rates.rx_drops_per_second
                );
                let _ = writeln!(
                    output,
                    "network.drops.tx.{interface} {:.2}",
                    rates.tx_drops_per_second
                );
                let _ = writeln!(
                    output,
                    "network.error_drop.rx.percent.{interface} {:.2}",
                    rates.rx_error_drop_percent
                );
                let _ = writeln!(
                    output,
                    "network.error_drop.tx.percent.{interface} {:.2}",
                    rates.tx_error_drop_percent
                );
            }
        }
        if let Some(Ok(uptime)) = uptime {
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

    fn status_threshold_exceeded(
        thresholds: &StatusThresholds,
        cpu: Option<&CpuMetrics>,
        memory: Option<&io::Result<Memory>>,
        network: Option<&io::Result<Option<NetworkSample>>>,
    ) -> bool {
        if let Some(cpu) = cpu {
            if thresholds
                .cpu_percent
                .is_some_and(|threshold| cpu.aggregate.usage >= threshold)
                || thresholds.iowait_percent.is_some_and(|threshold| {
                    cpu.aggregate.iowait.is_some_and(|value| value >= threshold)
                })
            {
                return true;
            }
        }

        if let Some(threshold) = thresholds.memory_percent {
            if memory
                .and_then(|result| result.as_ref().ok())
                .and_then(|memory| memory.total_bytes.map(|total| (memory.used_bytes, total)))
                .is_some_and(|(used, total)| {
                    total > 0 && used as f64 * 100.0 / total as f64 >= threshold
                })
            {
                return true;
            }
        }

        thresholds.nic_error_drop_percent.is_some_and(|threshold| {
            network
                .and_then(|result| result.as_ref().ok())
                .and_then(Option::as_ref)
                .and_then(|sample| {
                    sample.maximum_error_drop_percent(thresholds.network_min_packets)
                })
                .is_some_and(|value| value >= threshold)
        })
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
                    let rx_packets = counter_delta(current.rx_packets, previous.rx_packets);
                    let rx_errors = counter_delta(current.rx_errors, previous.rx_errors);
                    let rx_drops = counter_delta(current.rx_drops, previous.rx_drops);
                    let tx_packets = counter_delta(current.tx_packets, previous.tx_packets);
                    let tx_errors = counter_delta(current.tx_errors, previous.tx_errors);
                    let tx_drops = counter_delta(current.tx_drops, previous.tx_drops);
                    let rx_failed = rx_errors.saturating_add(rx_drops);
                    let tx_failed = tx_errors.saturating_add(tx_drops);
                    let rx_observed_packets = rx_packets.saturating_add(rx_failed);
                    let tx_observed_packets = tx_packets.saturating_add(tx_failed);
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
                            rx_packets_per_second: delta_rate(rx_packets, elapsed_seconds),
                            rx_error_drop_percent: counter_error_percent(
                                rx_failed,
                                rx_observed_packets,
                            ),
                            rx_observed_packets,
                            tx_packets_per_second: delta_rate(tx_packets, elapsed_seconds),
                            tx_error_drop_percent: counter_error_percent(
                                tx_failed,
                                tx_observed_packets,
                            ),
                            tx_observed_packets,
                            rx_errors_per_second: delta_rate(rx_errors, elapsed_seconds),
                            tx_errors_per_second: delta_rate(tx_errors, elapsed_seconds),
                            rx_drops_per_second: delta_rate(rx_drops, elapsed_seconds),
                            tx_drops_per_second: delta_rate(tx_drops, elapsed_seconds),
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

    fn counter_delta(current: u64, previous: u64) -> u64 {
        current.saturating_sub(previous)
    }

    fn delta_rate(delta: u64, elapsed_seconds: f64) -> f64 {
        delta as f64 / elapsed_seconds
    }

    fn counter_error_percent(errors_and_drops: u64, observed_packets: u64) -> f64 {
        if observed_packets == 0 {
            0.0
        } else {
            errors_and_drops as f64 * 100.0 / observed_packets as f64
        }
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

    fn start_cpu_sampler(
        metrics: Arc<Mutex<Option<CpuMetrics>>>,
        sample_error: Arc<AtomicBool>,
        source: CpuSamplerSource,
    ) -> mpsc::Receiver<()> {
        let (ready_sender, ready_receiver) = mpsc::channel();
        thread::Builder::new()
            .name("cpu-sampler".to_owned())
            .spawn(move || {
                let mut previous = match read_cpu_sample(&source) {
                    Ok(sample) => Some(TimedCpuSample {
                        sampled_at: Instant::now(),
                        sample,
                    }),
                    Err(_) => {
                        sample_error.store(true, Ordering::Relaxed);
                        None
                    }
                };
                let mut ready_sender = Some(ready_sender);
                loop {
                    thread::sleep(Duration::from_secs(1));
                    let sample = match read_cpu_sample(&source) {
                        Ok(sample) => {
                            sample_error.store(false, Ordering::Relaxed);
                            sample
                        }
                        Err(_) => {
                            sample_error.store(true, Ordering::Relaxed);
                            continue;
                        }
                    };
                    let current = TimedCpuSample {
                        sampled_at: Instant::now(),
                        sample,
                    };
                    if let Some(previous) = previous.as_ref() {
                        if let Some(cpu_metrics) = calculate_cpu_metrics(previous, &current) {
                            *metrics.lock().unwrap_or_else(|error| error.into_inner()) =
                                Some(cpu_metrics);
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

    fn read_cpu_sample(source: &CpuSamplerSource) -> io::Result<CpuSample> {
        match source {
            CpuSamplerSource::Proc => read_cpu_times().map(CpuSample::Proc),
            CpuSamplerSource::Cgroup(path) => {
                let (usage_ns, capacity_cores, cores) = read_cgroup_cpu(path)?;
                Ok(CpuSample::Cgroup {
                    usage_ns,
                    capacity_cores,
                    cores,
                })
            }
        }
    }

    fn calculate_cpu_metrics(
        previous: &TimedCpuSample,
        current: &TimedCpuSample,
    ) -> Option<CpuMetrics> {
        match (&previous.sample, &current.sample) {
            (CpuSample::Proc(previous), CpuSample::Proc(current)) => {
                let aggregate = calculate_cpu_percentages(previous.aggregate, current.aggregate)?;
                let cores = current
                    .cores
                    .iter()
                    .filter_map(|(core, counters)| {
                        let previous = previous.cores.get(core)?;
                        Some((*core, calculate_cpu_percentages(*previous, *counters)?))
                    })
                    .collect();
                Some(CpuMetrics { aggregate, cores })
            }
            (
                CpuSample::Cgroup {
                    usage_ns: previous_usage,
                    cores: previous_cores,
                    ..
                },
                CpuSample::Cgroup {
                    usage_ns: current_usage,
                    capacity_cores,
                    cores: current_cores,
                },
            ) => {
                let elapsed_ns = current
                    .sampled_at
                    .saturating_duration_since(previous.sampled_at)
                    .as_nanos() as f64;
                if elapsed_ns <= 0.0 || *capacity_cores <= 0.0 {
                    return None;
                }
                let usage_percent = current_usage.saturating_sub(*previous_usage) as f64 * 100.0
                    / (elapsed_ns * capacity_cores);
                let cores = previous_cores
                    .as_ref()
                    .zip(current_cores.as_ref())
                    .map(|(previous_cores, current_cores)| {
                        current_cores
                            .iter()
                            .filter_map(|(core, current_usage)| {
                                let previous_usage = previous_cores.get(core)?;
                                Some((
                                    *core,
                                    CpuPercentages {
                                        usage: (current_usage.saturating_sub(*previous_usage)
                                            as f64
                                            * 100.0
                                            / elapsed_ns)
                                            .clamp(0.0, 100.0),
                                        iowait: None,
                                    },
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(CpuMetrics {
                    aggregate: CpuPercentages {
                        usage: usage_percent.clamp(0.0, 100.0),
                        iowait: None,
                    },
                    cores,
                })
            }
            _ => None,
        }
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
            iowait: Some((iowait_delta as f64 * 100.0 / total_delta as f64).clamp(0.0, 100.0)),
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

    fn detect_runtime_environment(lxcfs_proc: LxcfsProcFiles) -> RuntimeEnvironment {
        let systemd_container = fs::read_to_string("/run/systemd/container")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let cgroup_text = fs::read_to_string("/proc/1/cgroup")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if systemd_container.contains("lxc")
            || cgroup_text.contains("/lxc/")
            || cgroup_text.contains("lxc.payload")
            || lxcfs_proc.stat
            || lxcfs_proc.meminfo
        {
            return RuntimeEnvironment::Lxc;
        }
        if Path::new("/.dockerenv").exists()
            || Path::new("/run/.containerenv").exists()
            || systemd_container.contains("docker")
            || systemd_container.contains("podman")
            || cgroup_text.contains("docker")
            || cgroup_text.contains("containerd")
            || cgroup_text.contains("kubepods")
            || cgroup_text.contains("libpod")
        {
            return RuntimeEnvironment::Container;
        }
        if is_virtual_machine() {
            RuntimeEnvironment::VirtualMachine
        } else {
            RuntimeEnvironment::Host
        }
    }

    fn is_virtual_machine() -> bool {
        if Path::new("/sys/hypervisor/type").exists() {
            return true;
        }
        let dmi = [
            "/sys/class/dmi/id/sys_vendor",
            "/sys/class/dmi/id/product_name",
            "/sys/class/dmi/id/board_vendor",
        ]
        .into_iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .collect::<String>()
        .to_ascii_lowercase();
        if [
            "kvm",
            "qemu",
            "vmware",
            "virtualbox",
            "xen",
            "bochs",
            "hyper-v",
            "virtual machine",
            "amazon ec2",
            "google",
            "openstack",
            "parallels",
            "bhyve",
        ]
        .iter()
        .any(|marker| dmi.contains(marker))
        {
            return true;
        }
        fs::read_to_string("/proc/cpuinfo").is_ok_and(|contents| {
            contents.lines().any(|line| {
                line.split_once(':').is_some_and(|(name, values)| {
                    matches!(name.trim(), "flags" | "Features")
                        && values
                            .split_ascii_whitespace()
                            .any(|value| value == "hypervisor")
                })
            })
        })
    }

    fn detect_lxcfs_proc_files() -> LxcfsProcFiles {
        let mut files = LxcfsProcFiles::default();
        let Ok(contents) = fs::read_to_string("/proc/self/mountinfo") else {
            return files;
        };
        for line in contents.lines() {
            let Some((mount_fields, fs_fields)) = line.split_once(" - ") else {
                continue;
            };
            let mount_fields = mount_fields.split_ascii_whitespace().collect::<Vec<_>>();
            let fs_fields = fs_fields.split_ascii_whitespace().collect::<Vec<_>>();
            if mount_fields.len() < 5 || fs_fields.len() < 2 {
                continue;
            }
            let is_lxcfs = fs_fields[0].to_ascii_lowercase().contains("lxcfs")
                || fs_fields[1].to_ascii_lowercase().contains("lxcfs");
            if !is_lxcfs {
                continue;
            }
            match unescape_mount_path(mount_fields[4])
                .to_string_lossy()
                .as_ref()
            {
                "/proc/stat" => files.stat = true,
                "/proc/meminfo" => files.meminfo = true,
                _ => {}
            }
        }
        files
    }

    fn unescape_mount_path(value: &str) -> PathBuf {
        let mut decoded = Vec::with_capacity(value.len());
        let bytes = value.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\\' && index + 3 < bytes.len() {
                let code = &value[index + 1..index + 4];
                let decoded_byte = match code {
                    "040" => Some(b' '),
                    "011" => Some(b'\t'),
                    "012" => Some(b'\n'),
                    "134" => Some(b'\\'),
                    _ => None,
                };
                if let Some(byte) = decoded_byte {
                    decoded.push(byte);
                    index += 4;
                    continue;
                }
            }
            decoded.push(bytes[index]);
            index += 1;
        }
        PathBuf::from(String::from_utf8_lossy(&decoded).into_owned())
    }

    fn read_cgroup_mounts() -> io::Result<Vec<CgroupMountInfo>> {
        let contents = fs::read_to_string("/proc/self/mountinfo")?;
        let mut mounts = Vec::new();
        for line in contents.lines() {
            let Some((mount_fields, fs_fields)) = line.split_once(" - ") else {
                continue;
            };
            let mount_fields = mount_fields.split_ascii_whitespace().collect::<Vec<_>>();
            let fs_fields = fs_fields.split_ascii_whitespace().collect::<Vec<_>>();
            if mount_fields.len() < 6 || fs_fields.len() < 3 {
                continue;
            }
            let kind = fs_fields[0];
            if kind != "cgroup" && kind != "cgroup2" {
                continue;
            }
            let mut controllers = BTreeSet::new();
            for option in [mount_fields[5], fs_fields[2]] {
                controllers.extend(
                    option
                        .split(',')
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned),
                );
            }
            mounts.push(CgroupMountInfo {
                kind: kind.to_owned(),
                root: unescape_mount_path(mount_fields[3]),
                mountpoint: unescape_mount_path(mount_fields[4]),
                controllers,
            });
        }
        Ok(mounts)
    }

    fn read_cgroup_memberships(pid: u32) -> io::Result<BTreeMap<String, String>> {
        let contents = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
        let mut memberships = BTreeMap::new();
        for line in contents.lines() {
            let Some((_, rest)) = line.split_once(':') else {
                continue;
            };
            let Some((controllers, path)) = rest.split_once(':') else {
                continue;
            };
            if controllers.is_empty() {
                memberships.insert("unified".to_owned(), path.to_owned());
            } else {
                for controller in controllers.split(',') {
                    memberships.insert(controller.to_owned(), path.to_owned());
                }
            }
        }
        Ok(memberships)
    }

    fn cgroup_mount_for<'a>(
        mounts: &'a [CgroupMountInfo],
        kind: &str,
        controller: Option<&str>,
    ) -> Option<&'a CgroupMountInfo> {
        mounts.iter().find(|mount| {
            mount.kind == kind
                && controller.is_none_or(|controller| mount.controllers.contains(controller))
        })
    }

    fn membership_for<'a>(
        memberships: &'a BTreeMap<String, String>,
        controller: &str,
    ) -> Option<&'a str> {
        memberships.get(controller).map(String::as_str)
    }

    fn map_cgroup_path(mount: &CgroupMountInfo, membership: &str) -> Option<PathBuf> {
        let relative = Path::new(membership).strip_prefix(&mount.root).ok()?;
        Some(mount.mountpoint.join(relative))
    }

    fn container_cgroup_dir(
        mount: &CgroupMountInfo,
        current_membership: &str,
        init_membership: &str,
    ) -> Option<PathBuf> {
        let current = map_cgroup_path(mount, current_membership)?;
        let init = map_cgroup_path(mount, init_membership)?;
        for path in [&current, &init] {
            if let Some(boundary) = lxc_cgroup_boundary(path) {
                if current.starts_with(&boundary) || init.starts_with(&boundary) {
                    return Some(boundary);
                }
            }
        }
        let mut candidate = current;
        while candidate.starts_with(&mount.mountpoint) {
            if init.starts_with(&candidate) {
                return Some(candidate);
            }
            if candidate == mount.mountpoint || !candidate.pop() {
                break;
            }
        }
        Some(mount.mountpoint.clone())
    }

    fn lxc_cgroup_boundary(path: &Path) -> Option<PathBuf> {
        let components = path
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .collect::<Vec<_>>();
        let lxc_index = components
            .iter()
            .position(|component| *component == "lxc")?;
        let name = components.get(lxc_index + 1)?;
        let mut boundary = PathBuf::new();
        for component in components.iter().take(lxc_index + 2) {
            boundary.push(component);
        }
        (!name.is_empty()).then_some(boundary)
    }

    fn discover_cgroup_resources() -> Option<CgroupResources> {
        let mounts = read_cgroup_mounts().ok()?;
        let current = read_cgroup_memberships(std::process::id()).ok()?;
        let init = read_cgroup_memberships(1).ok()?;
        let mut cpu = None;
        let mut memory = None;

        if let Some(mount) = cgroup_mount_for(&mounts, "cgroup2", None) {
            if let (Some(current_path), Some(init_path)) = (
                membership_for(&current, "unified"),
                membership_for(&init, "unified"),
            ) {
                if let Some(directory) = container_cgroup_dir(mount, current_path, init_path) {
                    if directory.join("cpu.stat").is_file() {
                        cpu = Some(CgroupCpuPath::V2(directory.clone()));
                    }
                    if directory.join("memory.current").is_file() {
                        memory = Some(CgroupMemoryPath::V2(directory));
                    }
                }
            }
        }

        if cpu.is_none() || memory.is_none() {
            let cpuacct_mount = cgroup_mount_for(&mounts, "cgroup", Some("cpuacct"));
            let cpu_mount = cgroup_mount_for(&mounts, "cgroup", Some("cpu"));
            let cpuset_mount = cgroup_mount_for(&mounts, "cgroup", Some("cpuset"));
            let memory_mount = cgroup_mount_for(&mounts, "cgroup", Some("memory"));
            let controller_dir = |mount: Option<&CgroupMountInfo>, controller: &str| {
                let mount = mount?;
                let current_path = membership_for(&current, controller)?;
                let init_path = membership_for(&init, controller)?;
                container_cgroup_dir(mount, current_path, init_path)
            };

            if cpu.is_none() {
                if let Some(cpuacct_dir) = controller_dir(cpuacct_mount, "cpuacct") {
                    if cpuacct_dir.join("cpuacct.usage").is_file() {
                        let cpu_dir = controller_dir(cpu_mount, "cpu");
                        let cpuset_dir = controller_dir(cpuset_mount, "cpuset");
                        cpu = Some(CgroupCpuPath::V1 {
                            cpu: cpu_dir,
                            cpuacct: cpuacct_dir,
                            cpuset: cpuset_dir,
                        });
                    }
                }
            }
            if memory.is_none() {
                if let Some(memory_dir) = controller_dir(memory_mount, "memory") {
                    if memory_dir.join("memory.usage_in_bytes").is_file() {
                        memory = Some(CgroupMemoryPath::V1(memory_dir));
                    }
                }
            }
        }

        (cpu.is_some() || memory.is_some()).then_some(CgroupResources { cpu, memory })
    }

    fn read_cgroup_cpu(
        path: &CgroupCpuPath,
    ) -> io::Result<(u64, f64, Option<BTreeMap<usize, u64>>)> {
        match path {
            CgroupCpuPath::V2(directory) => {
                let contents = fs::read_to_string(directory.join("cpu.stat"))?;
                let usage_usec = contents
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(' ')?;
                        (key == "usage_usec")
                            .then(|| value.trim().parse::<u64>().ok())
                            .flatten()
                    })
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "missing cpu.stat usage_usec")
                    })?;
                Ok((
                    usage_usec.saturating_mul(1_000),
                    cgroup_v2_capacity(directory),
                    None,
                ))
            }
            CgroupCpuPath::V1 {
                cpu,
                cpuacct,
                cpuset,
            } => {
                let usage_ns = read_u64_file(&cpuacct.join("cpuacct.usage"))?;
                let cores = read_u64_list_file(&cpuacct.join("cpuacct.usage_percpu"))
                    .ok()
                    .map(|usage| {
                        let possible = read_possible_cpu_ids(usage.len());
                        let allowed = cpuset
                            .as_deref()
                            .and_then(|directory| ancestor_cpuset_ids(directory, "cpuset.cpus"));
                        usage
                            .into_iter()
                            .enumerate()
                            .filter_map(|(index, usage_ns)| {
                                let core = possible.get(index).copied().unwrap_or(index);
                                if allowed
                                    .as_ref()
                                    .is_some_and(|allowed| !allowed.contains(&core))
                                {
                                    None
                                } else {
                                    Some((core, usage_ns))
                                }
                            })
                            .collect()
                    });
                Ok((
                    usage_ns,
                    cgroup_v1_capacity(cpu.as_deref(), cpuset.as_deref()),
                    cores,
                ))
            }
        }
    }

    fn read_cgroup_memory(path: &CgroupMemoryPath) -> io::Result<Memory> {
        let (used_bytes, configured_limit) = match path {
            CgroupMemoryPath::V2(directory) => {
                let used = read_u64_file(&directory.join("memory.current"))?;
                let limit = ancestor_memory_limit(directory, "memory.max", true);
                (used, limit)
            }
            CgroupMemoryPath::V1(directory) => {
                let used = read_u64_file(&directory.join("memory.usage_in_bytes"))?;
                let limit = ancestor_memory_limit(directory, "memory.limit_in_bytes", false);
                (used, limit)
            }
        };
        let total_bytes = configured_limit
            .filter(|limit| *limit > 0)
            .or_else(|| read_total_memory_bytes().ok());
        Ok(Memory {
            total_bytes,
            used_bytes,
            available_bytes: total_bytes.map(|total| total.saturating_sub(used_bytes)),
        })
    }

    fn ancestor_memory_limit(directory: &Path, filename: &str, v2: bool) -> Option<u64> {
        let mut current = Some(directory);
        let mut minimum = None::<u64>;
        while let Some(path) = current {
            if let Ok(value) = fs::read_to_string(path.join(filename)) {
                let limit = if v2 {
                    value.trim().parse::<u64>().ok()
                } else {
                    value
                        .trim()
                        .parse::<u64>()
                        .ok()
                        .filter(|value| *value < i64::MAX as u64 / 2)
                };
                if let Some(limit) = limit.filter(|value| *value > 0) {
                    minimum = Some(minimum.map_or(limit, |existing| existing.min(limit)));
                }
            }
            current = path.parent();
        }
        minimum
    }

    fn read_u64_file(path: &Path) -> io::Result<u64> {
        fs::read_to_string(path)?
            .trim()
            .parse::<u64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn read_u64_list_file(path: &Path) -> io::Result<Vec<u64>> {
        let contents = fs::read_to_string(path)?;
        let values = contents
            .split_ascii_whitespace()
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if values.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty per-core CPU counters",
            ));
        }
        Ok(values)
    }

    fn cgroup_v2_capacity(directory: &Path) -> f64 {
        let cpus = ancestor_cpuset_count(directory, "cpuset.cpus.effective")
            .or_else(|| ancestor_cpuset_count(directory, "cpuset.cpus"));
        let quota = ancestor_quota(directory, "cpu.max", true);
        effective_cpu_capacity(cpus, quota)
    }

    fn cgroup_v1_capacity(cpu: Option<&Path>, cpuset: Option<&Path>) -> f64 {
        let cpus = cpuset.and_then(|directory| ancestor_cpuset_count(directory, "cpuset.cpus"));
        let quota = cpu.and_then(|directory| ancestor_quota(directory, "cpu.cfs_quota_us", false));
        effective_cpu_capacity(cpus, quota)
    }

    fn effective_cpu_capacity(cpus: Option<f64>, quota: Option<f64>) -> f64 {
        match (cpus, quota) {
            (Some(cpus), Some(quota)) => cpus.min(quota).max(0.01),
            (Some(cpus), None) => cpus.max(0.01),
            (None, Some(quota)) => quota.max(0.01),
            (None, None) => std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1) as f64,
        }
    }

    fn ancestor_cpuset_count(directory: &Path, filename: &str) -> Option<f64> {
        let mut current = Some(directory);
        while let Some(path) = current {
            if let Ok(value) = fs::read_to_string(path.join(filename)) {
                if let Some(count) = parse_cpu_set_count(value.trim()) {
                    return Some(count as f64);
                }
            }
            current = path.parent();
        }
        None
    }

    fn ancestor_cpuset_ids(directory: &Path, filename: &str) -> Option<BTreeSet<usize>> {
        let mut current = Some(directory);
        while let Some(path) = current {
            if let Ok(value) = fs::read_to_string(path.join(filename)) {
                if let Some(ids) = parse_cpu_set_ids(value.trim()) {
                    return Some(ids);
                }
            }
            current = path.parent();
        }
        None
    }

    fn read_possible_cpu_ids(counter_count: usize) -> Vec<usize> {
        fs::read_to_string("/sys/devices/system/cpu/possible")
            .ok()
            .and_then(|value| parse_cpu_set_ids(value.trim()))
            .filter(|ids| ids.len() == counter_count)
            .map(|ids| ids.into_iter().collect())
            .unwrap_or_else(|| (0..counter_count).collect())
    }

    fn ancestor_quota(directory: &Path, filename: &str, v2: bool) -> Option<f64> {
        let mut current = Some(directory);
        let mut minimum = None::<f64>;
        while let Some(path) = current {
            if let Ok(contents) = fs::read_to_string(path.join(filename)) {
                let quota = if v2 {
                    let mut fields = contents.split_ascii_whitespace();
                    match (fields.next(), fields.next()) {
                        (Some("max"), _) | (None, _) => None,
                        (Some(quota), Some(period)) => quota
                            .parse::<f64>()
                            .ok()
                            .zip(period.parse::<f64>().ok())
                            .filter(|(quota, period)| *quota > 0.0 && *period > 0.0)
                            .map(|(quota, period)| quota / period),
                        _ => None,
                    }
                } else {
                    let quota = contents
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .filter(|value| *value > 0);
                    let period = fs::read_to_string(path.join("cpu.cfs_period_us"))
                        .ok()
                        .and_then(|value| value.trim().parse::<f64>().ok())
                        .filter(|value| *value > 0.0);
                    quota
                        .map(|value| value as f64)
                        .zip(period)
                        .map(|(quota, period)| quota / period)
                };
                if let Some(quota) = quota {
                    minimum = Some(minimum.map_or(quota, |existing| existing.min(quota)));
                }
            }
            current = path.parent();
        }
        minimum
    }

    fn parse_cpu_set_ids(value: &str) -> Option<BTreeSet<usize>> {
        if value.is_empty() {
            return None;
        }
        let mut ids = BTreeSet::new();
        for part in value.split(',') {
            if let Some((first, last)) = part.split_once('-') {
                let first = first.trim().parse::<usize>().ok()?;
                let last = last.trim().parse::<usize>().ok()?;
                if last < first || last.saturating_sub(first) > 65_536 {
                    return None;
                }
                ids.extend(first..=last);
            } else {
                ids.insert(part.trim().parse::<usize>().ok()?);
            }
        }
        (!ids.is_empty()).then_some(ids)
    }

    fn parse_cpu_set_count(value: &str) -> Option<usize> {
        parse_cpu_set_ids(value).map(|ids| ids.len())
    }

    fn read_total_memory_bytes() -> io::Result<u64> {
        let contents = fs::read_to_string("/proc/meminfo")?;
        let total_kib = contents
            .lines()
            .find_map(|line| {
                let (key, rest) = line.split_once(':')?;
                (key == "MemTotal")
                    .then(|| rest.split_ascii_whitespace().next()?.parse::<u64>().ok())
                    .flatten()
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing MemTotal"))?;
        Ok(total_kib.saturating_mul(1024))
    }

    fn memory_capacity_bytes(state: &State) -> Option<u64> {
        if state.environment.is_container() {
            if let Some(path) = state
                .cgroups
                .as_ref()
                .and_then(|resources| resources.memory.as_ref())
            {
                let configured_limit = match path {
                    CgroupMemoryPath::V2(directory) => {
                        ancestor_memory_limit(directory, "memory.max", true)
                    }
                    CgroupMemoryPath::V1(directory) => {
                        ancestor_memory_limit(directory, "memory.limit_in_bytes", false)
                    }
                };
                return configured_limit.or_else(|| read_total_memory_bytes().ok());
            }
            if !state.lxcfs_proc.meminfo {
                return None;
            }
        }
        read_total_memory_bytes().ok()
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
            used_bytes: total_kib.saturating_sub(available_kib).saturating_mul(1024),
            total_bytes: Some(total_kib.saturating_mul(1024)),
            available_bytes: Some(available_kib.saturating_mul(1024)),
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
