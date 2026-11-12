use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectorConfig {
    pub server_url: String,
    pub token: String,
    #[serde(default = "default_aw_url")]
    pub aw_url: String,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_lookback")]
    pub lookback_secs: i64,
    #[serde(default = "default_initial")]
    pub initial_sync_days: i64,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub aw_token: String,
    #[serde(default)]
    pub state_path: String,
}

fn default_aw_url() -> String {
    "http://127.0.0.1:5600".into()
}

fn default_poll() -> u64 {
    60
}

fn default_lookback() -> i64 {
    2 * 60 * 60
}

fn default_initial() -> i64 {
    30
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("activity-watch-manager")
        .join("collector.toml")
}

pub fn default_state_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("activity-watch-manager")
        .join("collector-state.json")
}

pub fn load_config(path: &Path) -> Result<CollectorConfig> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let mut cfg: CollectorConfig =
        toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
    normalize(&mut cfg);
    Ok(cfg)
}

pub fn write_new(path: &Path, server_url: &str, token: &str) -> Result<()> {
    if server_url.trim().is_empty() || token.trim().is_empty() {
        bail!("init needs --server and --token");
    }
    let mut cfg = CollectorConfig {
        server_url: server_url.trim().to_string(),
        token: token.trim().to_string(),
        aw_url: default_aw_url(),
        poll_interval_secs: default_poll(),
        lookback_secs: default_lookback(),
        initial_sync_days: default_initial(),
        hostname: String::new(),
        device_id: String::new(),
        aw_token: String::new(),
        state_path: String::new(),
    };
    normalize(&mut cfg);
    write_config(path, &cfg)
}

pub fn update_existing(path: &Path, server_url: Option<&str>, token: Option<&str>) -> Result<()> {
    let mut cfg = load_config(path)?;
    if let Some(server_url) = server_url {
        if !server_url.trim().is_empty() {
            cfg.server_url = server_url.trim().to_string();
        }
    }
    if let Some(token) = token {
        if !token.trim().is_empty() {
            cfg.token = token.trim().to_string();
        }
    }
    write_config(path, &cfg)
}

fn normalize(cfg: &mut CollectorConfig) {
    cfg.server_url = cfg.server_url.trim().trim_end_matches('/').to_string();
    cfg.aw_url = cfg.aw_url.trim().trim_end_matches('/').to_string();
    cfg.token = cfg.token.trim().to_string();
    cfg.poll_interval_secs = cfg.poll_interval_secs.clamp(15, 24 * 60 * 60);
    cfg.lookback_secs = cfg.lookback_secs.clamp(60, 7 * 24 * 60 * 60);
    cfg.initial_sync_days = cfg.initial_sync_days.clamp(1, 3650);
    if cfg.state_path.trim().is_empty() {
        cfg.state_path = default_state_path().to_string_lossy().into_owned();
    }
}

fn write_config(path: &Path, cfg: &CollectorConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        restrict_dir(parent);
    }
    let text = format!(
        r#"# activity-watch-manager collector
# Reads the local ActivityWatch API and reports to the central server.

server_url = {server_url}
token = {token}
aw_url = {aw_url}
# Set this if ActivityWatch itself has an API key enabled.
aw_token = {aw_token}
poll_interval_secs = {poll}
# Re-send this much recent history each poll so heartbeat merges update durations.
lookback_secs = {lookback}
initial_sync_days = {initial}
# Leave empty to use the hostname and device id ActivityWatch reports.
hostname = {hostname}
device_id = {device_id}
state_path = {state_path}
"#,
        server_url = toml_string(&cfg.server_url),
        token = toml_string(&cfg.token),
        aw_url = toml_string(&cfg.aw_url),
        aw_token = toml_string(&cfg.aw_token),
        poll = cfg.poll_interval_secs,
        lookback = cfg.lookback_secs,
        initial = cfg.initial_sync_days,
        hostname = toml_string(&cfg.hostname),
        device_id = toml_string(&cfg.device_id),
        state_path = toml_string(&cfg.state_path),
    );
    fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    restrict_file(path);
    Ok(())
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
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
