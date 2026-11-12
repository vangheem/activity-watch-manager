//! SQLite storage split into period files of about 30 days each.
//!
//! A UI query often crosses a boundary, so [`Store::summarize`] attaches every
//! period file the range can touch and reads them in one `UNION ALL`. Events
//! are stored in the file that contains their start timestamp. Reads also open
//! the previous file, because a long event can begin there and run into the
//! range. Heartbeat merges in ActivityWatch grow an event's duration without
//! changing its id, so ingest upserts on `(device_id, bucket_id, aw_event_id)`.

mod aggregate;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use awm_proto::{
    CategoryRule, IngestRequest, IngestResponse, MachineInfo, PeriodDb, StatusReport, Summary,
};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

const MAX_DURATION_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_DATA_JSON: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "period_days is {configured} but existing data uses {stored}. Refusing to mix period lengths"
    )]
    PeriodDaysMismatch { stored: i64, configured: i64 },
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub device_id: String,
    pub bucket_id: String,
    pub bucket_type: String,
    pub timestamp_ms: i64,
    pub duration_secs: f64,
    pub app: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub afk_status: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    period_days: i64,
    lock: Mutex<()>,
}

impl Store {
    pub fn open(data_dir: impl Into<PathBuf>, period_days: i64) -> Result<Self, StoreError> {
        if !(1..=365).contains(&period_days) {
            return Err(StoreError::Message(
                "period_days must be between 1 and 365".into(),
            ));
        }
        let data_dir = data_dir.into();
        fs::create_dir_all(&data_dir)?;
        fs::create_dir_all(data_dir.join("periods"))?;
        restrict_dir(&data_dir);
        restrict_dir(&data_dir.join("periods"));

        let store = Self {
            inner: Arc::new(Inner {
                data_dir,
                period_days,
                lock: Mutex::new(()),
            }),
        };
        store.ensure_period_days()?;
        Ok(store)
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    pub fn period_days(&self) -> i64 {
        self.inner.period_days
    }

    pub fn ingest(&self, request: &IngestRequest) -> Result<IngestResponse, StoreError> {
        let _guard = self.lock();
        let device_id = request.device_id.trim();
        if device_id.is_empty() || device_id.len() > 128 {
            return Err(StoreError::Message(
                "device_id is required and must be at most 128 characters".into(),
            ));
        }
        let hostname = truncate(request.hostname.trim(), 256);
        if hostname.is_empty() {
            return Err(StoreError::Message("hostname is required".into()));
        }

        let now = Utc::now().timestamp_millis();
        self.upsert_machine(device_id, &hostname, now)?;
        self.set_setting("last_ingest_ms", &now.to_string())?;

        let mut written = 0u64;
        let mut skipped = 0u64;
        let mut touched = Vec::new();

        let mut grouped: std::collections::BTreeMap<i64, Vec<Row>> =
            std::collections::BTreeMap::new();
        for bucket in &request.buckets {
            for event in &bucket.events {
                match prepare_row(device_id, bucket, event) {
                    Ok(row) => {
                        let period = period_start_ms(row.timestamp_ms, self.period_ms());
                        grouped.entry(period).or_default().push(row);
                    }
                    Err(()) => skipped += 1,
                }
            }
        }

        for (period, rows) in grouped {
            let path = self.period_path(period);
            let conn = self.open_period(&path)?;
            let tx = conn.unchecked_transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO events (
                        device_id, bucket_id, bucket_type, client, aw_event_id,
                        timestamp_ms, duration_secs, app, title, url, afk_status, data_json
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    ON CONFLICT(device_id, bucket_id, aw_event_id) DO UPDATE SET
                        bucket_type = excluded.bucket_type,
                        client = excluded.client,
                        timestamp_ms = excluded.timestamp_ms,
                        duration_secs = excluded.duration_secs,
                        app = excluded.app,
                        title = excluded.title,
                        url = excluded.url,
                        afk_status = excluded.afk_status,
                        data_json = excluded.data_json",
                )?;
                for row in &rows {
                    stmt.execute(params![
                        row.device_id,
                        row.bucket_id,
                        row.bucket_type,
                        row.client,
                        row.aw_event_id,
                        row.timestamp_ms,
                        row.duration_secs,
                        row.app,
                        row.title,
                        row.url,
                        row.afk_status,
                        row.data_json,
                    ])?;
                    written += 1;
                }
            }
            tx.commit()?;
            restrict_file(&path);
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                touched.push(name.to_string());
            }
        }

        Ok(IngestResponse {
            written,
            skipped,
            databases: touched,
        })
    }

    pub fn summarize(
        &self,
        start_ms: i64,
        end_ms: i64,
        device_id: Option<&str>,
        timezone: &str,
        categories: &[CategoryRule],
    ) -> Result<Summary, StoreError> {
        let _guard = self.lock();
        let (events, databases) = self.query_events(start_ms, end_ms, device_id)?;
        let machines = self.machines()?;
        let last_ingest_ms = self
            .setting("last_ingest_ms")?
            .and_then(|value| value.parse().ok());
        aggregate::build(aggregate::AggregateInput {
            events,
            machines,
            start_ms,
            end_ms,
            timezone: timezone.to_string(),
            categories: categories.to_vec(),
            databases,
            last_ingest_ms,
        })
        .map_err(StoreError::Message)
    }

    pub fn status(&self) -> Result<StatusReport, StoreError> {
        let _guard = self.lock();
        let mut databases = Vec::new();
        let mut total_events = 0i64;
        let mut total_bytes = 0u64;
        for path in self.list_period_files()? {
            let file = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown.sqlite")
                .to_string();
            let start_ms = period_start_from_name(&file).unwrap_or(0);
            let end_ms = start_ms + self.period_ms();
            let events = count_events(&path).unwrap_or(0);
            let bytes = file_bytes(&path);
            total_events += events;
            total_bytes += bytes;
            databases.push(PeriodDb {
                file,
                start: ms_rfc3339(start_ms),
                end: ms_rfc3339(end_ms),
                events,
                bytes,
            });
        }
        databases.sort_by(|a, b| a.start.cmp(&b.start));
        Ok(StatusReport {
            period_days: self.inner.period_days,
            total_events,
            total_bytes,
            last_ingest_ms: self
                .setting("last_ingest_ms")?
                .and_then(|value| value.parse().ok()),
            databases,
            machines: self.machines()?,
        })
    }

    fn query_events(
        &self,
        start_ms: i64,
        end_ms: i64,
        device_id: Option<&str>,
    ) -> Result<(Vec<StoredEvent>, Vec<String>), StoreError> {
        let lookback_start = start_ms.saturating_sub(MAX_DURATION_MS);
        let mut paths = Vec::new();
        let mut names = Vec::new();
        let mut period = period_start_ms(lookback_start, self.period_ms());
        let last = period_start_ms(end_ms.saturating_sub(1).max(start_ms), self.period_ms());
        while period <= last {
            let path = self.period_path(period);
            if path.exists() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    names.push(name.to_string());
                }
                paths.push(path);
            }
            period += self.period_ms();
        }
        if paths.is_empty() {
            return Ok((Vec::new(), names));
        }

        // One connection, every overlapping period file attached, one UNION ALL.
        // The UI range is whatever the person asked for; the files stay bounded.
        let conn = Connection::open_in_memory()?;
        conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, 125)?;
        let mut parts = Vec::with_capacity(paths.len());
        for (i, path) in paths.iter().enumerate() {
            let schema = format!("p{i}");
            conn.execute(
                &format!("ATTACH DATABASE ?1 AS {schema}"),
                [path.to_string_lossy().as_ref()],
            )
            .map_err(|source| {
                StoreError::Message(format!("failed to attach {}: {source}", path.display()))
            })?;
            parts.push(format!(
                "SELECT device_id, bucket_id, bucket_type, timestamp_ms, duration_secs, app, title, url, afk_status
                 FROM {schema}.events
                 WHERE timestamp_ms >= ?1 AND timestamp_ms < ?2
                   AND (?3 = '' OR device_id = ?3)"
            ));
        }
        let sql = parts.join(" UNION ALL ");
        let device = device_id.unwrap_or("");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![lookback_start, end_ms, device], |row| {
            Ok(StoredEvent {
                device_id: row.get(0)?,
                bucket_id: row.get(1)?,
                bucket_type: row.get(2)?,
                timestamp_ms: row.get(3)?,
                duration_secs: row.get(4)?,
                app: row.get(5)?,
                title: row.get(6)?,
                url: row.get(7)?,
                afk_status: row.get(8)?,
            })
        })?;
        let mut events = Vec::new();
        for row in rows {
            let event = row?;
            let end = event
                .timestamp_ms
                .saturating_add((event.duration_secs * 1000.0) as i64);
            if event.timestamp_ms < end_ms && end > start_ms {
                events.push(event);
            }
        }
        Ok((events, names))
    }

    fn upsert_machine(&self, device_id: &str, hostname: &str, now: i64) -> Result<(), StoreError> {
        let conn = self.open_meta()?;
        conn.execute(
            "INSERT INTO machines (device_id, hostname, first_seen_ms, last_seen_ms)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(device_id) DO UPDATE SET
                hostname = excluded.hostname,
                last_seen_ms = excluded.last_seen_ms",
            params![device_id, hostname, now],
        )?;
        Ok(())
    }

    fn machines(&self) -> Result<Vec<MachineInfo>, StoreError> {
        let conn = self.open_meta()?;
        let mut stmt = conn.prepare(
            "SELECT device_id, hostname, first_seen_ms, last_seen_ms
             FROM machines ORDER BY hostname",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MachineInfo {
                device_id: row.get(0)?,
                hostname: row.get(1)?,
                first_seen_ms: row.get(2)?,
                last_seen_ms: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    fn ensure_period_days(&self) -> Result<(), StoreError> {
        let conn = self.open_meta()?;
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'period_days'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(stored) = stored {
            let stored: i64 = stored.parse().unwrap_or(self.inner.period_days);
            if stored != self.inner.period_days {
                return Err(StoreError::PeriodDaysMismatch {
                    stored,
                    configured: self.inner.period_days,
                });
            }
        } else {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('period_days', ?1)",
                [self.inner.period_days.to_string()],
            )?;
        }
        Ok(())
    }

    fn setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let conn = self.open_meta()?;
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(StoreError::from)
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let conn = self.open_meta()?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn open_meta(&self) -> Result<Connection, StoreError> {
        let path = self.inner.data_dir.join("meta.sqlite");
        let conn = open_rw(&path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS machines (
                device_id TEXT PRIMARY KEY,
                hostname TEXT NOT NULL,
                first_seen_ms INTEGER NOT NULL,
                last_seen_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        restrict_file(&path);
        Ok(conn)
    }

    fn open_period(&self, path: &Path) -> Result<Connection, StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = open_rw(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                device_id TEXT NOT NULL,
                bucket_id TEXT NOT NULL,
                bucket_type TEXT NOT NULL,
                client TEXT NOT NULL DEFAULT '',
                aw_event_id INTEGER NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                duration_secs REAL NOT NULL,
                app TEXT,
                title TEXT,
                url TEXT,
                afk_status TEXT,
                data_json TEXT NOT NULL,
                PRIMARY KEY (device_id, bucket_id, aw_event_id)
            );
            CREATE INDEX IF NOT EXISTS idx_events_ts ON events(timestamp_ms);
            CREATE INDEX IF NOT EXISTS idx_events_device_ts ON events(device_id, timestamp_ms);",
        )?;
        Ok(conn)
    }

    fn list_period_files(&self) -> Result<Vec<PathBuf>, StoreError> {
        let dir = self.inner.data_dir.join("periods");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("sqlite") {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }

    fn period_path(&self, start_ms: i64) -> PathBuf {
        let name = DateTime::<Utc>::from_timestamp_millis(start_ms)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| format!("unknown-{start_ms}"));
        self.inner
            .data_dir
            .join("periods")
            .join(format!("{name}.sqlite"))
    }

    fn period_ms(&self) -> i64 {
        self.inner.period_days * 86_400_000
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

struct Row {
    device_id: String,
    bucket_id: String,
    bucket_type: String,
    client: String,
    aw_event_id: i64,
    timestamp_ms: i64,
    duration_secs: f64,
    app: Option<String>,
    title: Option<String>,
    url: Option<String>,
    afk_status: Option<String>,
    data_json: String,
}

fn prepare_row(
    device_id: &str,
    bucket: &awm_proto::IngestBucket,
    event: &awm_proto::IngestEvent,
) -> Result<Row, ()> {
    if event.id < 0 || !event.duration.is_finite() || event.duration < 0.0 {
        return Err(());
    }
    let Some(timestamp_ms) = parse_timestamp_ms(&event.timestamp) else {
        return Err(());
    };
    let extracted = extract_fields(&bucket.bucket_type, &event.data);
    let data_json = match serde_json::to_string(&event.data) {
        Ok(text) if text.len() <= MAX_DATA_JSON => text,
        _ => "{\"omitted\":\"payload too large\"}".into(),
    };
    Ok(Row {
        device_id: device_id.to_string(),
        bucket_id: truncate(bucket.id.trim(), 256),
        bucket_type: truncate(bucket.bucket_type.trim(), 128),
        client: truncate(bucket.client.trim(), 128),
        aw_event_id: event.id,
        timestamp_ms,
        duration_secs: event.duration,
        app: extracted.app,
        title: extracted.title,
        url: extracted.url,
        afk_status: extracted.afk_status,
        data_json,
    })
}

struct Extracted {
    app: Option<String>,
    title: Option<String>,
    url: Option<String>,
    afk_status: Option<String>,
}

fn extract_fields(bucket_type: &str, data: &serde_json::Value) -> Extracted {
    let text = |key: &str, max: usize| -> Option<String> {
        let value = data.get(key)?.as_str()?;
        let value = truncate(value.trim(), max);
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    };
    match bucket_type {
        "afkstatus" => Extracted {
            app: None,
            title: None,
            url: None,
            afk_status: text("status", 32),
        },
        "web.tab.current" => Extracted {
            app: None,
            title: text("title", 2000),
            url: text("url", 4000),
            afk_status: None,
        },
        "app.editor.activity" => Extracted {
            app: text("language", 64).or_else(|| text("project", 512)),
            title: text("file", 2000),
            url: None,
            afk_status: None,
        },
        _ => Extracted {
            app: text("app", 512),
            title: text("title", 2000),
            url: text("url", 4000),
            afk_status: text("status", 32),
        },
    }
}

fn parse_timestamp_ms(ts: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc().timestamp_millis());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive.and_utc().timestamp_millis());
    }
    None
}

fn period_start_ms(ts_ms: i64, period_ms: i64) -> i64 {
    ts_ms.div_euclid(period_ms) * period_ms
}

fn period_start_from_name(file: &str) -> Option<i64> {
    let stem = file.strip_suffix(".sqlite")?;
    let date = chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok()?;
    let dt = date.and_hms_opt(0, 0, 0)?.and_utc();
    Some(dt.timestamp_millis())
}

fn open_rw(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

fn count_events(path: &Path) -> Result<i64, StoreError> {
    let conn = Connection::open(path)?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    Ok(count)
}

fn file_bytes(path: &Path) -> u64 {
    let mut total = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        total += fs::metadata(sidecar).map(|meta| meta.len()).unwrap_or(0);
    }
    total
}

fn ms_rfc3339(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ms.to_string())
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        value.chars().take(max_chars).collect()
    }
}

fn restrict_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn restrict_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awm_proto::{IngestBucket, IngestEvent};
    use serde_json::json;

    fn rfc3339(ms: i64) -> String {
        DateTime::<Utc>::from_timestamp_millis(ms)
            .unwrap()
            .to_rfc3339()
    }

    fn ingest_event(id: i64, ms: i64, duration: f64, data: serde_json::Value) -> IngestEvent {
        IngestEvent {
            id,
            timestamp: rfc3339(ms),
            duration,
            data,
        }
    }

    #[test]
    fn period_split_is_queried_together_and_afk_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), 30).unwrap();
        let period_ms = 30 * 86_400_000;
        let boundary = 1_700_000_000_000i64.div_euclid(period_ms) * period_ms;

        let request = IngestRequest {
            device_id: "dev-1".into(),
            hostname: "mbp".into(),
            buckets: vec![
                IngestBucket {
                    id: "aw-watcher-window_mbp".into(),
                    bucket_type: "currentwindow".into(),
                    client: "aw-watcher-window".into(),
                    events: vec![
                        ingest_event(
                            1,
                            boundary - 3_600_000,
                            3600.0,
                            json!({"app": "Code", "title": "main.rs"}),
                        ),
                        ingest_event(
                            2,
                            boundary + 1_000,
                            1800.0,
                            json!({"app": "Safari", "title": "docs"}),
                        ),
                    ],
                },
                IngestBucket {
                    id: "aw-watcher-afk_mbp".into(),
                    bucket_type: "afkstatus".into(),
                    client: "aw-watcher-afk".into(),
                    events: vec![ingest_event(
                        9,
                        boundary - 1_800_000,
                        900.0,
                        json!({"status": "afk"}),
                    )],
                },
            ],
        };
        let written = store.ingest(&request).unwrap();
        assert_eq!(written.written, 3);
        assert_eq!(written.databases.len(), 2);

        let sqlite_files = fs::read_dir(dir.path().join("periods"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("sqlite"))
            .count();
        assert_eq!(sqlite_files, 2);

        let summary = store
            .summarize(
                boundary - 86_400_000,
                boundary + 86_400_000,
                None,
                "UTC",
                &[],
            )
            .unwrap();
        assert_eq!(summary.databases.len(), 2);
        // 3600s window minus 900s AFK, plus the 1800s window in the next file.
        assert!((summary.active_secs - 4500.0).abs() < 0.01);
        assert!(summary.apps.iter().any(|app| app.app == "Code"));
        assert!(summary.apps.iter().any(|app| app.app == "Safari"));
        assert_eq!(summary.machines.len(), 1);
        assert_eq!(summary.machines[0].hostname, "mbp");
    }

    #[test]
    fn upsert_replaces_duration_without_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), 30).unwrap();
        let start = 1_700_000_000_000i64;
        let bucket = |duration| IngestRequest {
            device_id: "dev-1".into(),
            hostname: "mbp".into(),
            buckets: vec![IngestBucket {
                id: "aw-watcher-window_mbp".into(),
                bucket_type: "currentwindow".into(),
                client: "aw-watcher-window".into(),
                events: vec![ingest_event(
                    7,
                    start,
                    duration,
                    json!({"app": "Code", "title": "lib.rs"}),
                )],
            }],
        };
        store.ingest(&bucket(100.0)).unwrap();
        store.ingest(&bucket(200.0)).unwrap();
        let status = store.status().unwrap();
        assert_eq!(status.total_events, 1);
        let summary = store
            .summarize(start - 1_000, start + 86_400_000, None, "UTC", &[])
            .unwrap();
        assert!((summary.active_secs - 200.0).abs() < 0.01);
    }
}
