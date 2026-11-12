mod config;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use awm_proto::{IngestBucket, IngestEvent, IngestRequest, IngestResponse};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use config::CollectorConfig;
use serde::Deserialize;
use tracing::{error, info, warn};

const BATCH: usize = 1000;

#[derive(Parser, Debug)]
#[command(
    name = "awm-collector",
    version,
    about = "Report local ActivityWatch data to awm-server"
)]
struct Cli {
    /// Config file. Defaults to the Application Support location.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Sync once and exit.
    #[arg(long)]
    once: bool,
    /// Override the server URL for this run, and for init or install.
    #[arg(long, global = true)]
    server: Option<String>,
    /// Override the server token for this run, and for init or install.
    #[arg(long, global = true)]
    token: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Write collector.toml. Requires --server and --token the first time.
    Init,
    /// Print the config and state paths.
    Paths,
    /// Register a login item, start it now, and keep it running.
    Install,
    /// Remove the login item. Keeps config unless --purge is set.
    Uninstall {
        /// Also delete the collector config and sync cursor.
        #[arg(long)]
        purge: bool,
    },
    /// Show whether collection is installed, running, and reaching both sides.
    Status,
}

#[derive(Debug, Deserialize)]
struct AwInfo {
    #[serde(default)]
    hostname: String,
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    version: String,
}

#[derive(Debug, Deserialize)]
struct AwBucket {
    #[serde(default, rename = "type")]
    bucket_type: String,
    #[serde(default)]
    client: String,
}

#[derive(Debug, Deserialize)]
struct AwEvent {
    id: i64,
    timestamp: String,
    duration: f64,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SyncState {
    #[serde(default)]
    synced_until: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(config::config_path);
    match cli.command {
        Some(Command::Paths) => {
            println!("config: {}", config_path.display());
            let state = if config_path.exists() {
                config::load_config(&config_path)?.state_path
            } else {
                config::default_state_path().to_string_lossy().into_owned()
            };
            println!("state: {state}");
            return Ok(());
        }
        Some(Command::Init) => {
            if config_path.exists() {
                config::update_existing(&config_path, cli.server.as_deref(), cli.token.as_deref())?;
                println!("config: {}", config_path.display());
                println!("created: no");
            } else {
                config::write_new(
                    &config_path,
                    cli.server.as_deref().unwrap_or(""),
                    cli.token.as_deref().unwrap_or(""),
                )?;
                println!("config: {}", config_path.display());
                println!("created: yes");
            }
            return Ok(());
        }
        Some(Command::Install) => {
            install_service(&config_path, cli.server.as_deref(), cli.token.as_deref()).await?;
            return Ok(());
        }
        Some(Command::Uninstall { purge }) => {
            uninstall_service(&config_path, purge)?;
            return Ok(());
        }
        Some(Command::Status) => {
            let ok = print_status(&config_path).await?;
            if !ok {
                std::process::exit(1);
            }
            return Ok(());
        }
        None => {}
    }

    init_tracing();
    if !config_path.exists() {
        anyhow::bail!(
            "no config at {}. Run `awm-collector install --server URL --token TOKEN`.",
            config_path.display()
        );
    }
    let mut cfg = config::load_config(&config_path)?;
    if let Some(server) = cli.server.as_deref() {
        if !server.trim().is_empty() {
            cfg.server_url = server.trim().trim_end_matches('/').to_string();
        }
    }
    if let Some(token) = cli.token.as_deref() {
        if !token.trim().is_empty() {
            cfg.token = token.trim().to_string();
        }
    }
    if cfg.server_url.is_empty() || cfg.token.is_empty() {
        anyhow::bail!("server_url and token are required");
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(concat!("awm-collector/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let state_path = PathBuf::from(&cfg.state_path);
    info!(
        server = %cfg.server_url,
        aw = %cfg.aw_url,
        every_secs = cfg.poll_interval_secs,
        "collector started"
    );

    let mut failed = false;
    loop {
        let started = Instant::now();
        match sync_once(&cfg, &http, &state_path).await {
            Ok(stats) => info!(
                events = stats.events,
                written = stats.written,
                skipped = stats.skipped,
                buckets = stats.buckets,
                ms = started.elapsed().as_millis() as u64,
                "synced"
            ),
            Err(err) => {
                failed = true;
                error!("{err:#}");
            }
        }
        if cli.once {
            break;
        }
        failed = false;
        tokio::select! {
            _ = shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(cfg.poll_interval_secs)) => {}
        }
    }
    if failed && cli.once {
        std::process::exit(1);
    }
    Ok(())
}

struct SyncStats {
    events: usize,
    written: u64,
    skipped: u64,
    buckets: usize,
}

async fn sync_once(
    cfg: &CollectorConfig,
    http: &reqwest::Client,
    state_path: &Path,
) -> Result<SyncStats> {
    let info = aw_get::<AwInfo>(http, cfg, "/api/0/info")
        .await
        .context("ActivityWatch is not running, or aw_url is wrong")?;
    let hostname = first_nonempty([&cfg.hostname, &info.hostname]).unwrap_or_else(local_hostname);
    let device_id =
        first_nonempty([&cfg.device_id, &info.device_id]).unwrap_or_else(|| hostname.clone());
    tracing::debug!(
        version = %info.version,
        host = %hostname,
        device = %device_id,
        "activitywatch"
    );

    let buckets: std::collections::HashMap<String, AwBucket> =
        aw_get(http, cfg, "/api/0/buckets/").await?;
    if buckets.is_empty() {
        warn!("ActivityWatch returned no buckets");
    }

    let state = load_state(state_path);
    let now = Utc::now();
    let from = state
        .synced_until
        .as_deref()
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.with_timezone(&Utc) - chrono::Duration::seconds(cfg.lookback_secs))
        .unwrap_or_else(|| now - chrono::Duration::days(cfg.initial_sync_days));

    let mut collected = Vec::new();
    let mut event_count = 0usize;
    for (id, bucket) in &buckets {
        let events = fetch_events(http, cfg, id, from, now).await?;
        event_count += events.len();
        if events.is_empty() {
            continue;
        }
        collected.push(IngestBucket {
            id: id.clone(),
            bucket_type: infer_type(id, &bucket.bucket_type),
            client: bucket.client.clone(),
            events,
        });
    }

    let mut written = 0u64;
    let mut skipped = 0u64;
    let batches = batch_buckets(collected);
    for batch in batches {
        let response = post_ingest(
            http,
            cfg,
            &IngestRequest {
                device_id: device_id.clone(),
                hostname: hostname.clone(),
                buckets: batch,
            },
        )
        .await?;
        written += response.written;
        skipped += response.skipped;
    }

    let mut state = state;
    state.synced_until = Some(now.to_rfc3339());
    save_state(state_path, &state)?;
    Ok(SyncStats {
        events: event_count,
        written,
        skipped,
        buckets: buckets.len(),
    })
}

async fn fetch_events(
    http: &reqwest::Client,
    cfg: &CollectorConfig,
    bucket_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<IngestEvent>> {
    let mut events = Vec::new();
    let mut cursor = from;
    let step = chrono::Duration::days(7);
    while cursor < to {
        let end = (cursor + step).min(to);
        let path = format!("/api/0/buckets/{}/events", urlencoding::encode(bucket_id));
        let url = format!("{}{path}", cfg.aw_url);
        let mut req = http.get(&url).query(&[
            ("start", cursor.to_rfc3339()),
            ("end", end.to_rfc3339()),
            ("limit", "-1".into()),
        ]);
        if !cfg.aw_token.is_empty() {
            req = req.bearer_auth(&cfg.aw_token);
        }
        let response = req
            .send()
            .await
            .with_context(|| format!("ActivityWatch is not reachable at {}", cfg.aw_url))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "ActivityWatch bucket {bucket_id} returned {status}: {}",
                trim_body(&body)
            );
        }
        let batch: Vec<AwEvent> = response
            .json()
            .await
            .with_context(|| format!("unexpected events payload for {bucket_id}"))?;
        events.extend(batch.into_iter().map(|event| IngestEvent {
            id: event.id,
            timestamp: event.timestamp,
            duration: event.duration,
            data: event.data,
        }));
        cursor = end;
    }
    Ok(events)
}

fn batch_buckets(buckets: Vec<IngestBucket>) -> Vec<Vec<IngestBucket>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut count = 0usize;
    for bucket in buckets {
        let IngestBucket {
            id,
            bucket_type,
            client,
            mut events,
        } = bucket;
        while !events.is_empty() {
            if count == BATCH {
                out.push(std::mem::take(&mut current));
                count = 0;
            }
            let room = BATCH - count;
            let take = room.min(events.len());
            let chunk: Vec<_> = events.drain(..take).collect();
            count += chunk.len();
            current.push(IngestBucket {
                id: id.clone(),
                bucket_type: bucket_type.clone(),
                client: client.clone(),
                events: chunk,
            });
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

async fn post_ingest(
    http: &reqwest::Client,
    cfg: &CollectorConfig,
    body: &IngestRequest,
) -> Result<IngestResponse> {
    let url = format!("{}/api/v1/ingest", cfg.server_url);
    let response = http
        .post(&url)
        .bearer_auth(&cfg.token)
        .json(body)
        .send()
        .await
        .with_context(|| format!("server is not reachable at {}", cfg.server_url))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("server rejected the token");
    }
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("server returned {status}: {}", trim_body(&text));
    }
    response
        .json()
        .await
        .context("server returned an unexpected ingest response")
}

async fn aw_get<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    cfg: &CollectorConfig,
    path: &str,
) -> Result<T> {
    let url = format!("{}{path}", cfg.aw_url);
    let mut req = http.get(&url);
    if !cfg.aw_token.is_empty() {
        req = req.bearer_auth(&cfg.aw_token);
    }
    let response = req
        .send()
        .await
        .with_context(|| format!("ActivityWatch is not reachable at {}", cfg.aw_url))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "ActivityWatch {path} returned {status}: {}",
            trim_body(&body)
        ));
    }
    response
        .json()
        .await
        .with_context(|| format!("ActivityWatch {path} returned unexpected JSON"))
}

fn infer_type(id: &str, explicit: &str) -> String {
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    let id = id.to_ascii_lowercase();
    if id.contains("afk") {
        "afkstatus".into()
    } else if id.contains("web") {
        "web.tab.current".into()
    } else if id.contains("window") {
        "currentwindow".into()
    } else {
        "unknown".into()
    }
}

fn first_nonempty(values: [&str; 2]) -> Option<String> {
    values
        .into_iter()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

fn local_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown-host".into())
}

fn load_state(path: &Path) -> SyncState {
    let Ok(text) = std::fs::read_to_string(path) else {
        return SyncState::default();
    };
    match serde_json::from_str(&text) {
        Ok(state) => state,
        Err(err) => {
            warn!(path = %path.display(), "{err}");
            SyncState::default()
        }
    }
}

fn save_state(path: &Path, state: &SyncState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn trim_body(body: &str) -> String {
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > 180 {
        format!("{}…", compact.chars().take(180).collect::<String>())
    } else {
        compact
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
}

async fn install_service(
    config_path: &Path,
    server: Option<&str>,
    token: Option<&str>,
) -> Result<()> {
    let (server, token) = resolve_endpoint(config_path, server, token)?;
    if config_path.exists() {
        config::update_existing(config_path, Some(&server), Some(&token))?;
    } else {
        config::write_new(config_path, &server, &token)?;
    }
    let program = awm_service::service_program("awm-collector")?;
    awm_service::install(&awm_service::ServiceSpec::collector(
        program,
        vec![
            "--config".into(),
            config_path.to_string_lossy().into_owned(),
        ],
    ))?;
    println!("Installed. awm-collector starts at login and restarts if it exits.");
    println!("  Server  {server}");
    println!("  Config  {}", config_path.display());
    let cfg = config::load_config(config_path)?;
    match probe_aw(&cfg).await {
        Ok(line) => println!("  ActivityWatch  {line}"),
        Err(err) => println!("  ActivityWatch  down  {err}"),
    }
    println!("Check it with: awm-collector status");
    Ok(())
}

fn uninstall_service(config_path: &Path, purge: bool) -> Result<()> {
    let state_path = config_path
        .exists()
        .then(|| config::load_config(config_path).ok())
        .flatten()
        .map(|cfg| cfg.state_path);
    awm_service::uninstall(awm_service::COLLECTOR_LABEL)?;
    println!("Removed the login item.");
    if purge {
        let _ = std::fs::remove_file(config_path);
        if let Some(state_path) = state_path {
            let _ = std::fs::remove_file(state_path);
        }
        let (out_log, err_log) = awm_service::log_paths("collector");
        let _ = std::fs::remove_file(out_log);
        let _ = std::fs::remove_file(err_log);
        println!("Deleted the collector config, cursor, and logs.");
    } else {
        println!("Config was kept. Re-run with --purge to delete it.");
    }
    Ok(())
}

async fn print_status(config_path: &Path) -> Result<bool> {
    println!("awm-collector");
    let service = awm_service::query(awm_service::COLLECTOR_LABEL);
    let service_text = if service.running {
        match service.pid {
            Some(pid) => format!("running  pid {pid}"),
            None => "running".into(),
        }
    } else if service.installed {
        format!("installed, {}", service.state)
    } else {
        "not installed  (run `awm-collector install`)".into()
    };
    row("service", &service_text);

    let cfg = match config::load_config(config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            row("config", &format!("unreadable ({err})"));
            row("result", "not ok");
            return Ok(false);
        }
    };
    row("config", &config_path.display().to_string());
    row("server", &cfg.server_url);
    row("activitywatch", &cfg.aw_url);

    let aw_ok = match probe_aw(&cfg).await {
        Ok(line) => {
            row("aw", &line);
            true
        }
        Err(err) => {
            row("aw", &format!("down  {err}"));
            false
        }
    };
    let server_ok = match probe_server(&cfg.server_url).await {
        Ok(line) => {
            row("server health", &line);
            true
        }
        Err(err) => {
            row("server health", &format!("down  {err}"));
            false
        }
    };

    let state = load_state(Path::new(&cfg.state_path));
    let sync_recent = match state.synced_until.as_deref() {
        Some(ts) => match DateTime::parse_from_rfc3339(ts) {
            Ok(dt) => {
                let age = Utc::now().signed_duration_since(dt.with_timezone(&Utc));
                let secs = age.num_seconds().max(0);
                row("last sync", &format!("{}  ({})", ago(secs), ts));
                secs <= cfg.poll_interval_secs as i64 * 3
            }
            Err(_) => {
                row("last sync", "unreadable cursor");
                false
            }
        },
        None => {
            row("last sync", "never");
            false
        }
    };

    let (out_log, _) = awm_service::log_paths("collector");
    row("log", &out_log.display().to_string());
    for line in awm_service::tail(&out_log, 3) {
        println!("                 {line}");
    }

    let ok = service.running && aw_ok && server_ok && sync_recent;
    row("result", if ok { "ok" } else { "not ok" });
    Ok(ok)
}

fn resolve_endpoint(
    config_path: &Path,
    server: Option<&str>,
    token: Option<&str>,
) -> Result<(String, String)> {
    let from_flags = (
        server.map(str::trim).filter(|s| !s.is_empty()),
        token.map(str::trim).filter(|s| !s.is_empty()),
    );
    if let (Some(server), Some(token)) = from_flags {
        return Ok((server.trim_end_matches('/').to_string(), token.to_string()));
    }
    if config_path.exists() {
        let mut cfg = config::load_config(config_path)?;
        if let Some(server) = from_flags.0 {
            cfg.server_url = server.trim_end_matches('/').to_string();
        }
        if let Some(token) = from_flags.1 {
            cfg.token = token.to_string();
        }
        if !cfg.server_url.is_empty() && !cfg.token.is_empty() {
            return Ok((cfg.server_url, cfg.token));
        }
    }
    if let Some((server, token)) = local_server_endpoint() {
        return Ok((
            from_flags
                .0
                .unwrap_or(server.as_str())
                .trim_end_matches('/')
                .to_string(),
            from_flags.1.unwrap_or(token.as_str()).to_string(),
        ));
    }
    anyhow::bail!(
        "pass --server and --token, or run this on the server Mac after `awm-server install`"
    )
}

fn local_server_endpoint() -> Option<(String, String)> {
    let path = dirs::config_dir()?.join("activity-watch-manager/server.toml");
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let token = value.get("token")?.as_str()?.trim().to_string();
    if token.is_empty() {
        return None;
    }
    let listen = value
        .get("listen")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0.0:8787");
    let port = listen.rsplit(':').next().unwrap_or("8787");
    Some((format!("http://127.0.0.1:{port}"), token))
}

async fn probe_aw(cfg: &CollectorConfig) -> Result<String, String> {
    let client = status_client()?;
    let url = format!("{}/api/0/info", cfg.aw_url);
    let mut req = client.get(url);
    if !cfg.aw_token.is_empty() {
        req = req.bearer_auth(&cfg.aw_token);
    }
    let response = req.send().await.map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let info: AwInfo = response.json().await.map_err(|err| err.to_string())?;
    Ok(format!(
        "ok  {}  {}",
        if info.version.is_empty() {
            "unknown version"
        } else {
            info.version.as_str()
        },
        if info.hostname.is_empty() {
            "unknown host"
        } else {
            info.hostname.as_str()
        }
    ))
}

async fn probe_server(server_url: &str) -> Result<String, String> {
    let client = status_client()?;
    let response = client
        .get(format!("{server_url}/api/v1/health"))
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    Ok(format!("ok  {server_url}"))
}

fn status_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|err| err.to_string())
}

fn row(label: &str, value: &str) {
    println!("  {label:<16} {value}");
}

fn ago(secs: i64) -> String {
    if secs < 45 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 48 * 3600 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

async fn shutdown_signal() {
    let ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
