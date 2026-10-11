use std::{
    fs::{self, File},
    io::{self, BufReader},
    path::{Path, PathBuf},
};

use atomicwrites::{AllowOverwrite, AtomicFile};
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::{http_probe::HttpTimings, monitor::CheckResult};

const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    chunk_minutes: u32,
    retention_days: u32,
}

impl Store {
    pub fn open(
        path: impl AsRef<Path>,
        chunk_minutes: u32,
        retention_days: u32,
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
        let store = Self {
            root,
            chunk_minutes,
            retention_days,
        };
        store.delete_expired_sync(Utc::now().timestamp_millis())?;
        Ok(store)
    }

    pub async fn insert_check(&self, result: &CheckResult) -> io::Result<()> {
        validate_monitor_id(&result.monitor_id)?;
        let root = self.root.clone();
        let chunk_minutes = self.chunk_minutes;
        let record = CheckRecord::from(result);
        tokio::task::spawn_blocking(move || {
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

            let mut records = match read_chunk(&path) {
                Ok(records) => records,
                Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(error),
            };
            records.push(record);
            write_chunk(&path, &records)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn history(&self, monitor_id: &str, limit: usize) -> io::Result<Vec<CheckRecord>> {
        validate_monitor_id(monitor_id)?;
        if limit == 0 {
            return Ok(Vec::new());
        }

        let root = self.root.clone();
        let monitor_id = monitor_id.to_owned();
        let retention_days = self.retention_days;
        tokio::task::spawn_blocking(move || {
            let cutoff_ms = cutoff_millis(retention_days);
            let monitor_dir = root.join(monitor_id);
            let mut chunks = collect_chunk_paths(&monitor_dir)?;
            chunks.sort_unstable_by(|left, right| right.cmp(left));

            let mut history = Vec::with_capacity(limit);
            for path in chunks {
                let mut records = read_chunk(&path)?;
                records.sort_unstable_by_key(|record| record.checked_at_ms);
                for record in records.into_iter().rev() {
                    if record.checked_at_ms < cutoff_ms {
                        continue;
                    }
                    history.push(record);
                    if history.len() == limit {
                        return Ok(history);
                    }
                }
            }
            Ok(history)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn normal_percent(
        &self,
        monitor_id: &str,
        days: u32,
        slow_response_threshold_ms: Option<u64>,
    ) -> io::Result<Option<f64>> {
        validate_monitor_id(monitor_id)?;
        let root = self.root.clone();
        let monitor_id = monitor_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let cutoff_ms = cutoff_millis(days);
            let chunks = collect_chunk_paths(&root.join(monitor_id))?;
            let mut total = 0u64;
            let mut normal = 0u64;
            for path in chunks {
                for record in read_chunk(&path)? {
                    if record.checked_at_ms >= cutoff_ms {
                        total += 1;
                        let slow = slow_response_threshold_ms.is_some_and(|threshold| {
                            record
                                .latency_ms
                                .is_some_and(|latency| latency > threshold as f64)
                        }) || record.is_degraded;
                        normal += u64::from(record.is_up && !slow);
                    }
                }
            }
            Ok((total != 0).then(|| normal as f64 * 100.0 / total as f64))
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn delete_expired(&self) -> io::Result<()> {
        let root = self.root.clone();
        let cutoff_ms = cutoff_millis(self.retention_days);
        tokio::task::spawn_blocking(move || delete_expired_from(&root, cutoff_ms))
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

fn write_chunk(path: &Path, records: &[CheckRecord]) -> io::Result<()> {
    AtomicFile::new(path, AllowOverwrite)
        .write(|file| {
            ciborium::into_writer(records, file)
                .map_err(|error| io::Error::other(error.to_string()))
        })
        .map_err(|error| io::Error::other(error.to_string()))
}

fn collect_chunk_paths(monitor_dir: &Path) -> io::Result<Vec<PathBuf>> {
    if !monitor_dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for year_entry in fs::read_dir(monitor_dir)? {
        let year_entry = year_entry?;
        if !year_entry.file_type()?.is_dir() {
            continue;
        }
        for date_entry in fs::read_dir(year_entry.path())? {
            let date_entry = date_entry?;
            if !date_entry.file_type()?.is_dir() {
                continue;
            }
            paths.extend(collect_day_chunks(&date_entry.path())?);
        }
    }
    Ok(paths)
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
