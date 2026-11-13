//! Wire types shared by the collector, the server, and the UI.
//!
//! Field names are snake_case on the wire. The collector posts [`IngestRequest`];
//! the UI reads [`Summary`] and [`StatusReport`].

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestRequest {
    pub device_id: String,
    pub hostname: String,
    pub buckets: Vec<IngestBucket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestBucket {
    pub id: String,
    #[serde(rename = "type")]
    pub bucket_type: String,
    #[serde(default)]
    pub client: String,
    #[serde(default)]
    pub events: Vec<IngestEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestEvent {
    pub id: i64,
    pub timestamp: String,
    pub duration: f64,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResponse {
    pub written: u64,
    pub skipped: u64,
    pub databases: Vec<String>,
}

/// App grouping used by the UI. Loaded from the server config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryRule {
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub start: String,
    pub end: String,
    pub timezone: String,
    pub active_secs: f64,
    pub afk_secs: f64,
    pub event_count: i64,
    /// Period files the query attached and read together.
    pub databases: Vec<String>,
    pub last_ingest_ms: Option<i64>,
    pub machines: Vec<MachineTotal>,
    pub apps: Vec<AppTotal>,
    pub categories: Vec<CategoryTotal>,
    pub titles: Vec<TitleTotal>,
    pub sites: Vec<SiteTotal>,
    /// Individual browser pages from the web watcher, not mixed into window titles.
    pub pages: Vec<PageTotal>,
    pub days: Vec<DaySummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineTotal {
    pub device_id: String,
    pub hostname: String,
    pub last_seen_ms: i64,
    pub active_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppTotal {
    pub app: String,
    pub secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryTotal {
    pub name: String,
    pub color: String,
    pub secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleTotal {
    pub app: String,
    pub title: String,
    pub secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteTotal {
    pub domain: String,
    pub secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageTotal {
    pub title: String,
    pub url: String,
    pub domain: String,
    pub secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaySummary {
    /// Local calendar date in the requested timezone, `YYYY-MM-DD`.
    pub date: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub active_secs: f64,
    pub afk_secs: f64,
    /// Mini ribbon. Per-app when a single machine is in view, otherwise the
    /// union of time any machine was active.
    pub segments: Vec<Segment>,
    pub lanes: Vec<Lane>,
    pub titles: Vec<TitleTotal>,
    pub sites: Vec<SiteTotal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lane {
    pub device_id: String,
    pub hostname: String,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub start_ms: i64,
    pub end_ms: i64,
    /// `app` or `afk`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub period_days: i64,
    pub total_events: i64,
    pub total_bytes: u64,
    pub last_ingest_ms: Option<i64>,
    pub databases: Vec<PeriodDb>,
    pub machines: Vec<MachineInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeriodDb {
    pub file: String,
    pub start: String,
    pub end: String,
    pub events: i64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineInfo {
    pub device_id: String,
    pub hostname: String,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}
