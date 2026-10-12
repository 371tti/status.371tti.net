use std::{
    fs::{self, File},
    io::{self, BufReader},
    mem::size_of,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use atomicwrites::{AllowOverwrite, AtomicFile};
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use moka::sync::Cache;
use serde::{Deserialize, Serialize};

use crate::{http_probe::HttpTimings, monitor::CheckResult};

const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
const BYTES_PER_MIB: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    chunk_minutes: u32,
    retention_days: u32,
    chunk_cache: Cache<PathBuf, Arc<Vec<CheckRecord>>>,
    chunk_io_lock: Arc<RwLock<()>>,
    cache_enabled: bool,
}

impl Store {
    pub fn open(
        path: impl AsRef<Path>,
        chunk_minutes: u32,
        retention_days: u32,
        cache_max_mb: u32,
    ) -> io::Result<Self> {
        if !(1..=1440).contains(&chunk_minutes) || 1440 % chunk_minutes != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "chunk_minutes must divide evenly into a day",
            ));
        }
        if retention_days == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "retention_days must be greater than zero",
            ));
        }
        let root = path.as_ref().to_owned();
        fs::create_dir_all(&root)?;
        let cache_max_bytes = u64::from(cache_max_mb) * BYTES_PER_MIB;
        let chunk_cache = Cache::builder()
            .max_capacity(cache_max_bytes)
            .weigher(|_path: &PathBuf, records: &Arc<Vec<CheckRecord>>| {
                estimated_chunk_memory_bytes(records)
            })
            .build();
        let store = Self {
            root,
            chunk_minutes,
            retention_days,
            chunk_cache,
            chunk_io_lock: Arc::new(RwLock::new(())),
            cache_enabled: cache_max_mb > 0,
        };
        store.delete_expired_sync(Utc::now().timestamp_millis())?;
        Ok(store)
    }

    pub async fn insert_check(&self, result: &CheckResult) -> io::Result<()> {
        validate_monitor_id(&result.monitor_id)?;
        let root = self.root.clone();
        let chunk_minutes = self.chunk_minutes;
        let chunk_cache = self.chunk_cache.clone();
        let chunk_io_lock = Arc::clone(&self.chunk_io_lock);
        let cache_enabled = self.cache_enabled;
        let record = CheckRecord::from(result);
        tokio::task::spawn_blocking(move || {
            let _chunk_io_guard = chunk_io_lock
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let path = chunk_path(
                &root,
                &record.monitor_id,
                record.checked_at_ms,
                chunk_minutes,
            )?;
            let parent = path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "record path has no parent")
            })?;
            fs::create_dir_all(parent)?;

            let mut records = match cache_enabled.then(|| chunk_cache.get(&path)).flatten() {
                Some(records) => records.as_ref().clone(),
                None => match read_chunk(&path) {
                    Ok(records) => records,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                    Err(error) => return Err(error),
                },
            };
            records.push(record);
            write_chunk(&path, &records)?;
            if cache_enabled {
                chunk_cache.insert(path, Arc::new(records));
                chunk_cache.run_pending_tasks();
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn history_range(
        &self,
        monitor_id: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> io::Result<Vec<CheckRecord>> {
        validate_monitor_id(monitor_id)?;
        let start_ms = start_ms.max(cutoff_millis(self.retention_days));
        let end_ms = end_ms.min(Utc::now().timestamp_millis());
        if start_ms >= end_ms {
            return Ok(Vec::new());
        }

        let root = self.root.clone();
        let monitor_id = monitor_id.to_owned();
        let chunk_cache = self.chunk_cache.clone();
        let chunk_io_lock = Arc::clone(&self.chunk_io_lock);
        let cache_enabled = self.cache_enabled;
        tokio::task::spawn_blocking(move || {
            let monitor_dir = root.join(monitor_id);
            if !monitor_dir.exists() {
                return Ok(Vec::new());
            }

            let mut history = Vec::new();
            for year_entry in fs::read_dir(&monitor_dir)? {
                let year_entry = year_entry?;
                if !year_entry.file_type()?.is_dir() {
                    continue;
                }
                for date_entry in fs::read_dir(year_entry.path())? {
                    let date_entry = date_entry?;
                    if !date_entry.file_type()?.is_dir() {
                        continue;
                    }
                    let Some(date_name) = date_entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    let Ok(date) = NaiveDate::parse_from_str(&date_name, "%Y-%m-%d") else {
                        continue;
                    };
                    let Some(day_start) = date
                        .and_hms_opt(0, 0, 0)
                        .map(|datetime| datetime.and_utc().timestamp_millis())
                    else {
                        continue;
                    };
                    let day_end = day_start.saturating_add(MILLIS_PER_DAY);
                    if day_end <= start_ms || day_start >= end_ms {
                        continue;
                    }
                    for path in collect_day_chunks(&date_entry.path())? {
                        let records = {
                            let _chunk_io_guard = chunk_io_lock
                                .read()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if cache_enabled {
                                match chunk_cache.get(&path) {
                                    Some(records) => records,
                                    None => {
                                        let records = Arc::new(read_chunk(&path)?);
                                        chunk_cache.insert(path.clone(), Arc::clone(&records));
                                        records
                                    }
                                }
                            } else {
                                Arc::new(read_chunk(&path)?)
                            }
                        };
                        history.extend(
                            records
                                .iter()
                                .filter(|record| {
                                    record.checked_at_ms >= start_ms
                                        && record.checked_at_ms < end_ms
                                })
                                .cloned(),
                        );
                    }
                }
            }
            if cache_enabled {
                chunk_cache.run_pending_tasks();
            }
            history.sort_unstable_by(|left, right| right.checked_at_ms.cmp(&left.checked_at_ms));
            Ok(history)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn delete_expired(&self) -> io::Result<()> {
        let root = self.root.clone();
        let cutoff_ms = cutoff_millis(self.retention_days);
        let chunk_cache = self.chunk_cache.clone();
        let chunk_io_lock = Arc::clone(&self.chunk_io_lock);
        tokio::task::spawn_blocking(move || {
            let _chunk_io_guard = chunk_io_lock
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            delete_expired_from(&root, cutoff_ms)?;
            chunk_cache.invalidate_all();
            chunk_cache.run_pending_tasks();
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    fn delete_expired_sync(&self, now_ms: i64) -> io::Result<()> {
        delete_expired_from(
            &self.root,
            now_ms - i64::from(self.retention_days) * MILLIS_PER_DAY,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckRecord {
    pub monitor_id: String,
    pub checked_at_ms: i64,
    pub is_up: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_degraded: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_pending: bool,
    pub latency_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_timings: Option<HttpTimings>,
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_status_code: Option<i64>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metrics: std::collections::BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metric_units: std::collections::BTreeMap<String, String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_response: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tcp_response_truncated: bool,
}

impl From<&CheckResult> for CheckRecord {
    fn from(result: &CheckResult) -> Self {
        Self {
            monitor_id: result.monitor_id.clone(),
            checked_at_ms: result.checked_at_ms,
            is_up: result.is_up,
            is_degraded: result.is_degraded,
            is_pending: false,
            latency_ms: result.latency_ms,
            http_timings: result.http_timings.clone(),
            status_code: result.status_code,
            service_status_code: result.service_status_code,
            metrics: result.metrics.clone(),
            metric_units: result.metric_units.clone(),
            message: result.message.clone(),
            tcp_response: result.tcp_response.clone(),
            tcp_response_truncated: result.tcp_response_truncated,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

fn validate_monitor_id(monitor_id: &str) -> io::Result<()> {
    if monitor_id.is_empty()
        || monitor_id.split('.').any(str::is_empty)
        || !monitor_id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.'
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "monitor id contains unsupported path characters",
        ));
    }
    Ok(())
}

fn cutoff_millis(days: u32) -> i64 {
    Utc::now().timestamp_millis() - i64::from(days) * MILLIS_PER_DAY
}

fn chunk_path(
    root: &Path,
    monitor_id: &str,
    checked_at_ms: i64,
    chunk_minutes: u32,
) -> io::Result<PathBuf> {
    let checked_at = DateTime::<Utc>::from_timestamp_millis(checked_at_ms)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid check timestamp"))?;
    let date = checked_at.date_naive();
    let minute_of_day = checked_at.hour() * 60 + checked_at.minute();
    let bucket_start = minute_of_day / chunk_minutes * chunk_minutes;
    let bucket_hour = bucket_start / 60;
    let bucket_minute = bucket_start % 60;
    let file_name = format!("{bucket_hour:02}{bucket_minute:02}.cbor");
    Ok(root
        .join(monitor_id)
        .join(date.format("%Y").to_string())
        .join(date.format("%Y-%m-%d").to_string())
        .join(file_name))
}

fn read_chunk(path: &Path) -> io::Result<Vec<CheckRecord>> {
    let file = File::open(path)?;
    ciborium::from_reader(BufReader::new(file)).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid CBOR in {}: {error}", path.display()),
        )
    })
}

fn estimated_chunk_memory_bytes(records: &Arc<Vec<CheckRecord>>) -> u32 {
    let map_link_overhead = 4 * size_of::<usize>();
    let mut bytes = size_of::<Arc<Vec<CheckRecord>>>()
        .saturating_add(size_of::<Vec<CheckRecord>>())
        .saturating_add(records.capacity().saturating_mul(size_of::<CheckRecord>()));
    for record in records.iter() {
        bytes = bytes
            .saturating_add(record.monitor_id.capacity())
            .saturating_add(record.message.capacity())
            .saturating_add(record.tcp_response.as_ref().map_or(0, Vec::capacity));
        for key in record.metrics.keys() {
            bytes = bytes
                .saturating_add(size_of::<(String, f64)>())
                .saturating_add(map_link_overhead)
                .saturating_add(key.capacity());
        }
        for (key, value) in &record.metric_units {
            bytes = bytes
                .saturating_add(size_of::<(String, String)>())
                .saturating_add(map_link_overhead)
                .saturating_add(key.capacity())
                .saturating_add(value.capacity());
        }
    }
    bytes.clamp(1, u32::MAX as usize) as u32
}

fn write_chunk(path: &Path, records: &[CheckRecord]) -> io::Result<()> {
    AtomicFile::new(path, AllowOverwrite)
        .write(|file| {
            ciborium::into_writer(records, file)
                .map_err(|error| io::Error::other(error.to_string()))
        })
        .map_err(|error| io::Error::other(error.to_string()))
}

fn collect_day_chunks(day_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(day_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "cbor")
        {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

fn delete_expired_from(root: &Path, cutoff_ms: i64) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let cutoff_date = DateTime::<Utc>::from_timestamp_millis(cutoff_ms)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid retention cutoff"))?
        .date_naive();

    for monitor_entry in fs::read_dir(root)? {
        let monitor_entry = monitor_entry?;
        if !monitor_entry.file_type()?.is_dir() {
            continue;
        }
        for year_entry in fs::read_dir(monitor_entry.path())? {
            let year_entry = year_entry?;
            if !year_entry.file_type()?.is_dir() {
                continue;
            }
            for date_entry in fs::read_dir(year_entry.path())? {
                let date_entry = date_entry?;
                if !date_entry.file_type()?.is_dir() {
                    continue;
                }
                let Some(date_name) = date_entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(date) = NaiveDate::parse_from_str(&date_name, "%Y-%m-%d") else {
                    continue;
                };
                if date < cutoff_date {
                    fs::remove_dir_all(date_entry.path())?;
                } else if date == cutoff_date {
                    prune_day(&date_entry.path(), cutoff_ms)?;
                }
            }
            remove_if_empty(&year_entry.path())?;
        }
        remove_if_empty(&monitor_entry.path())?;
    }
    Ok(())
}

fn prune_day(day_dir: &Path, cutoff_ms: i64) -> io::Result<()> {
    for path in collect_day_chunks(day_dir)? {
        let mut records = read_chunk(&path)?;
        records.retain(|record| record.checked_at_ms >= cutoff_ms);
        if records.is_empty() {
            fs::remove_file(path)?;
        } else {
            write_chunk(&path, &records)?;
        }
    }
    remove_if_empty(day_dir)
}

fn remove_if_empty(path: &Path) -> io::Result<()> {
    let mut entries = fs::read_dir(path)?;
    if entries.next().transpose()?.is_none() {
        fs::remove_dir(path)?;
    }
    Ok(())
}
