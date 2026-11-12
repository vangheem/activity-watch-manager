mod config;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use awm_store::Store;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use clap::{Parser, Subcommand};
use config::ServerConfig;
use serde::Deserialize;
use tower_http::compression::CompressionLayer;
use tracing::info;

const INDEX: &str = include_str!("../assets/index.html");
const CSS: &str = include_str!("../assets/app.css");
const JS: &str = include_str!("../assets/app.js");
const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#1c1712"/><path d="M8 11h16M8 16h12M8 21h14" stroke="#e08a45" stroke-width="2" stroke-linecap="round"/></svg>"##;

#[derive(Parser, Debug)]
#[command(name = "awm-server", version, about = "Activity Watch Manager server")]
struct Cli {
    /// Config file. Defaults to the Application Support location.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Write a default config if one does not exist.
    Init,
    /// Print the API token.
    Token,
    /// Print config and data paths.
    Paths,
    /// Register a login item, start it now, and keep it running.
    Install,
    /// Remove the login item. Keeps data unless --purge is set.
    Uninstall {
        /// Also delete the config, databases, and logs.
        #[arg(long)]
        purge: bool,
    },
    /// Show whether the server is installed, running, and answering.
    Status,
}

#[derive(Clone)]
struct AppState {
    store: Store,
    config: Arc<ServerConfig>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(config::config_path);
    match cli.command {
        Some(Command::Paths) => {
            print_paths(&config_path)?;
            return Ok(());
        }
        Some(Command::Token) => {
            let cfg = config::load_config(&config_path)?;
            println!("{}", cfg.token);
            return Ok(());
        }
        Some(Command::Init) => {
            let created = config::ensure_config(&config_path)?;
            println!("config: {}", config_path.display());
            println!("created: {}", if created { "yes" } else { "no" });
            return Ok(());
        }
        Some(Command::Install) => {
            install_service(&config_path)?;
            return Ok(());
        }
        Some(Command::Uninstall { purge }) => {
            uninstall_service(&config_path, purge)?;
            return Ok(());
        }
        Some(Command::Status) => {
            let ok = print_status(&config_path)?;
            if !ok {
                std::process::exit(1);
            }
            return Ok(());
        }
        None => {}
    }

    init_tracing();
    let created = config::ensure_config(&config_path)?;
    if created {
        info!(path = %config_path.display(), "wrote a new config with a generated token");
    }
    let cfg = config::load_config(&config_path)?;
    if cfg.token.len() < 16 {
        tracing::warn!("token is shorter than 16 characters");
    }
    let addr: SocketAddr = cfg
        .listen
        .parse()
        .with_context(|| format!("listen address {}", cfg.listen))?;
    let store = Store::open(&cfg.data_dir, cfg.period_days)?;
    let state = AppState {
        store,
        config: Arc::new(cfg),
    };
    let app = router(state).layer(CompressionLayer::new());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("UI at http://127.0.0.1:{}", addr.port());
    if !addr.ip().is_loopback() {
        info!(%addr, "collectors on other machines need the token");
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/app.css", get(css))
        .route("/assets/app.js", get(js))
        .route("/favicon.svg", get(favicon))
        .route("/api/v1/health", get(health))
        .route("/api/v1/ingest", post(ingest))
        .route("/api/v1/summary", get(summary))
        .route("/api/v1/status", get(status))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .with_writer(std::io::stdout)
        .init();
}

fn print_paths(config_path: &std::path::Path) -> Result<()> {
    println!("config: {}", config_path.display());
    if config_path.exists() {
        let cfg = config::load_config(config_path)?;
        println!("data: {}", cfg.data_dir);
    } else {
        println!("data: {}", config::default_data_dir().display());
    }
    Ok(())
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

async fn auth(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = request.uri().path();
    let protected = path.starts_with("/api/") && path != "/api/v1/health";
    if protected && !allowed(&state, &request, path) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

fn allowed(state: &AppState, request: &axum::extract::Request, path: &str) -> bool {
    if bearer_ok(request, &state.config.token) {
        return true;
    }
    // Ingest always needs the token. Reads from this machine do not, so the
    // browser on the server host opens with no prompt.
    if path.starts_with("/api/v1/ingest") {
        return false;
    }
    if !state.config.trust_localhost {
        return false;
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|info| info.0.ip().is_loopback())
}

fn bearer_ok(request: &axum::extract::Request, expected: &str) -> bool {
    let Some(header) = request.headers().get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = header.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.trim(), expected)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    asset(INDEX, "text/html; charset=utf-8")
}

async fn css() -> Response {
    asset(CSS, "text/css; charset=utf-8")
}

async fn js() -> Response {
    asset(JS, "text/javascript; charset=utf-8")
}

async fn favicon() -> Response {
    asset(FAVICON, "image/svg+xml")
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn ingest(
    State(state): State<AppState>,
    Json(body): Json<awm_proto::IngestRequest>,
) -> Result<Json<awm_proto::IngestResponse>, ApiError> {
    let store = state.store.clone();
    let device = body.device_id.clone();
    let host = body.hostname.clone();
    let response = tokio::task::spawn_blocking(move || store.ingest(&body))
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "ingest task failed"))?
        .map_err(ApiError::from_store)?;
    info!(
        device = %device,
        host = %host,
        written = response.written,
        skipped = response.skipped,
        databases = ?response.databases,
        "ingest"
    );
    Ok(Json(response))
}

#[derive(Deserialize)]
struct SummaryParams {
    start: Option<String>,
    end: Option<String>,
    device: Option<String>,
    tz: Option<String>,
}

async fn summary(
    State(state): State<AppState>,
    Query(params): Query<SummaryParams>,
) -> Result<Json<awm_proto::Summary>, ApiError> {
    let tz_name = params
        .tz
        .as_deref()
        .map(str::trim)
        .filter(|tz| !tz.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let configured = state.config.timezone.trim();
            if configured.is_empty() {
                None
            } else {
                Some(configured.to_string())
            }
        })
        .unwrap_or_else(system_tz);
    let tz: Tz = tz_name.parse().unwrap_or(chrono_tz::UTC);
    let (start_ms, end_ms) = match (&params.start, &params.end) {
        (Some(start), Some(end)) => {
            let start_ms = parse_ms(start)
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "start must be RFC3339"))?;
            let end_ms = parse_ms(end)
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "end must be RFC3339"))?;
            (start_ms, end_ms)
        }
        _ => today_bounds(tz).ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, "could not resolve today's bounds")
        })?,
    };
    if end_ms <= start_ms {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "start must be before end",
        ));
    }
    let device = params
        .device
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let store = state.store.clone();
    let categories = state.config.categories.clone();
    let tz_used = tz.to_string();
    let report = tokio::task::spawn_blocking(move || {
        store.summarize(start_ms, end_ms, device.as_deref(), &tz_used, &categories)
    })
    .await
    .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "summary task failed"))?
    .map_err(ApiError::from_store)?;
    Ok(Json(report))
}

async fn status(State(state): State<AppState>) -> Result<Json<awm_proto::StatusReport>, ApiError> {
    let store = state.store.clone();
    let report = tokio::task::spawn_blocking(move || store.status())
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "status task failed"))?
        .map_err(ApiError::from_store)?;
    Ok(Json(report))
}

async fn not_found(uri: axum::http::Uri) -> Response {
    if uri.path().starts_with("/api/") {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response()
    } else {
        (StatusCode::NOT_FOUND, "not found").into_response()
    }
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn from_store(err: awm_store::StoreError) -> Self {
        let status = match &err {
            awm_store::StoreError::Message(_) => StatusCode::BAD_REQUEST,
            awm_store::StoreError::PeriodDaysMismatch { .. } => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({"error": self.message})),
        )
            .into_response()
    }
}

fn parse_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn system_tz() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into())
}

fn today_bounds(tz: Tz) -> Option<(i64, i64)> {
    let today = Utc::now().with_timezone(&tz).date_naive();
    let start = midnight(today, tz)?;
    let end = midnight(today.succ_opt()?, tz)?;
    Some((start, end))
}

fn midnight(date: NaiveDate, tz: Tz) -> Option<i64> {
    let naive = date.and_hms_opt(0, 0, 0)?;
    let local = tz
        .from_local_datetime(&naive)
        .single()
        .or_else(|| tz.from_local_datetime(&naive).earliest())?;
    Some(local.timestamp_millis())
}

fn install_service(config_path: &std::path::Path) -> Result<()> {
    let created = config::ensure_config(config_path)?;
    let cfg = config::load_config(config_path)?;
    let program = awm_service::service_program("awm-server")?;
    awm_service::install(&awm_service::ServiceSpec::server(
        program,
        service_args(config_path),
    ))?;
    let ready = wait_health(&cfg.listen);
    println!("Installed. awm-server starts at login and restarts if it exits.");
    println!("  UI      {}", ui_origin(&cfg.listen));
    println!("  Token   {}", cfg.token);
    println!("  Config  {}", config_path.display());
    if created {
        println!("  A new token was written to the config.");
    }
    if let Some(ip) = lan_ip() {
        let port = listen_port(&cfg.listen);
        println!();
        println!("On each Mac that should report activity:");
        println!("  brew install vangheem/tap/awm-collector");
        println!(
            "  awm-collector install --server http://{ip}:{port} --token {}",
            cfg.token
        );
    }
    println!();
    if ready {
        println!("Health check passed. `awm-server status` shows the details.");
    } else {
        println!(
            "It did not answer yet. If macOS asked to allow incoming connections, click Allow,"
        );
        println!("then run `awm-server status`.");
    }
    Ok(())
}

fn uninstall_service(config_path: &std::path::Path, purge: bool) -> Result<()> {
    let data_dir = config::load_config(config_path)
        .ok()
        .map(|cfg| cfg.data_dir);
    awm_service::uninstall(awm_service::SERVER_LABEL)?;
    println!("Removed the login item.");
    if purge {
        if let Some(data_dir) = data_dir {
            let _ = std::fs::remove_dir_all(data_dir);
        }
        let _ = std::fs::remove_file(config_path);
        let (out_log, err_log) = awm_service::log_paths("server");
        let _ = std::fs::remove_file(out_log);
        let _ = std::fs::remove_file(err_log);
        println!("Deleted the server config, databases, and logs.");
    } else {
        println!("Config and databases were kept. Re-run with --purge to delete them.");
    }
    Ok(())
}

fn print_status(config_path: &std::path::Path) -> Result<bool> {
    println!("awm-server");
    let service = awm_service::query(awm_service::SERVER_LABEL);
    let service_text = if service.running {
        match service.pid {
            Some(pid) => format!("running  pid {pid}"),
            None => "running".into(),
        }
    } else if service.installed {
        format!("installed, {}", service.state)
    } else {
        "not installed  (run `awm-server install`)".into()
    };
    row("service", &service_text);

    let cfg = match config::load_config(config_path) {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            row("config", &format!("unreadable ({err})"));
            row("result", "not ok");
            return Ok(false);
        }
    };
    let Some(cfg) = cfg else {
        return Ok(false);
    };
    row("config", &config_path.display().to_string());
    row("listen", &cfg.listen);
    row("token", &mask_token(&cfg.token));

    let health = probe_health(&cfg.listen);
    let health_ok = health.is_ok();
    row(
        "health",
        &health.unwrap_or_else(|err| format!("down  {err}")),
    );

    match Store::open(&cfg.data_dir, cfg.period_days).and_then(|store| store.status()) {
        Ok(status) => {
            row(
                "storage",
                &format!(
                    "{} databases  {} events  {}",
                    status.databases.len(),
                    status.total_events,
                    format_bytes(status.total_bytes)
                ),
            );
            row(
                "last ingest",
                &status
                    .last_ingest_ms
                    .map(ago)
                    .unwrap_or_else(|| "never".into()),
            );
            if status.machines.is_empty() {
                row("machines", "none yet");
            } else {
                row("machines", &status.machines.len().to_string());
                for machine in &status.machines {
                    println!(
                        "                 {}  last seen {}",
                        machine.hostname,
                        ago(machine.last_seen_ms)
                    );
                }
            }
        }
        Err(err) => row("storage", &format!("unavailable ({err})")),
    }

    let (out_log, _) = awm_service::log_paths("server");
    row("log", &out_log.display().to_string());
    for line in awm_service::tail(&out_log, 3) {
        println!("                 {line}");
    }

    let ok = service.running && health_ok;
    row("result", if ok { "ok" } else { "not ok" });
    Ok(ok)
}

fn service_args(config_path: &std::path::Path) -> Vec<String> {
    vec![
        "--config".into(),
        config_path.to_string_lossy().into_owned(),
    ]
}

fn probe_health(listen: &str) -> Result<String, String> {
    let addr: SocketAddr = listen
        .parse()
        .map_err(|_| format!("bad listen address {listen}"))?;
    let host = if addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else if addr.ip().is_ipv6() {
        format!("[{}]", addr.ip())
    } else {
        addr.ip().to_string()
    };
    match http_get(&host, addr.port(), "/api/v1/health") {
        Ok((200, _)) => Ok(format!("ok  {}", ui_origin(listen))),
        Ok((status, _)) => Err(format!("HTTP {status}")),
        Err(err) => Err(err),
    }
}

fn wait_health(listen: &str) -> bool {
    for _ in 0..20 {
        if probe_health(listen).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    false
}

fn http_get(host: &str, port: u16, path: &str) -> Result<(u16, String), String> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|_| format!("bad address {host}:{port}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))
        .map_err(|err| err.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .ok();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|err| err.to_string())?;
    let mut buf = String::new();
    stream
        .read_to_string(&mut buf)
        .map_err(|err| err.to_string())?;
    let mut lines = buf.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, buf))
}

fn ui_origin(listen: &str) -> String {
    let port = listen_port(listen);
    format!("http://127.0.0.1:{port}")
}

fn listen_port(listen: &str) -> u16 {
    listen
        .parse::<SocketAddr>()
        .map(|addr| addr.port())
        .unwrap_or(8787)
}

fn lan_ip() -> Option<String> {
    for iface in ["en0", "en1"] {
        let output = std::process::Command::new("ipconfig")
            .args(["getifaddr", iface])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let ip = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if !ip.is_empty() {
            return Some(ip);
        }
    }
    None
}

fn row(label: &str, value: &str) {
    println!("  {label:<12} {value}");
}

fn mask_token(token: &str) -> String {
    if token.len() <= 8 {
        return "set".into();
    }
    format!("{}…{}", &token[..4], &token[token.len() - 4..])
}

fn ago(ms: i64) -> String {
    let secs = ((Utc::now().timestamp_millis() - ms) / 1000).max(0);
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

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1024.0 / 1024.0)
    }
}
