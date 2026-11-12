//! Install `awm-server` and `awm-collector` so they start at login and come back
//! if they exit. macOS uses a LaunchAgent. Linux uses a systemd user service.
//!
//! The plist or unit points at Homebrew's `opt/<formula>/bin` symlink when the
//! binary is installed that way, so `brew upgrade` updates what the next start
//! runs. Re-run `install` after an upgrade to restart the already-running process.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub const SERVER_LABEL: &str = "com.activitywatch.manager.server";
pub const COLLECTOR_LABEL: &str = "com.activitywatch.manager.collector";

#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub label: String,
    pub formula: String,
    /// Log file stem: `server` or `collector`.
    pub log_name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub nice: i32,
}

impl ServiceSpec {
    pub fn server(program: PathBuf, args: Vec<String>) -> Self {
        Self {
            label: SERVER_LABEL.into(),
            formula: "awm-server".into(),
            log_name: "server".into(),
            program,
            args,
            nice: 0,
        }
    }

    pub fn collector(program: PathBuf, args: Vec<String>) -> Self {
        Self {
            label: COLLECTOR_LABEL.into(),
            formula: "awm-collector".into(),
            log_name: "collector".into(),
            program,
            args,
            nice: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceState {
    pub installed: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub state: String,
}

pub fn install(spec: &ServiceSpec) -> Result<()> {
    let (out_log, err_log) = log_paths(&spec.log_name);
    if let Some(parent) = out_log.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(target_os = "macos")]
    {
        install_launchd(spec, &out_log, &err_log)
    }
    #[cfg(target_os = "linux")]
    {
        install_systemd(spec, &out_log, &err_log)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (spec, out_log, err_log);
        bail!("install is supported on macOS and Linux");
    }
}

pub fn uninstall(label: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        uninstall_launchd(label)
    }
    #[cfg(target_os = "linux")]
    {
        uninstall_systemd(label)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = label;
        bail!("uninstall is supported on macOS and Linux");
    }
}

pub fn query(label: &str) -> ServiceState {
    #[cfg(target_os = "macos")]
    {
        query_launchd(label)
    }
    #[cfg(target_os = "linux")]
    {
        query_systemd(label)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = label;
        ServiceState {
            installed: false,
            running: false,
            pid: None,
            state: "unsupported".into(),
        }
    }
}

/// Path the login item should execute. Homebrew cellar paths are rewritten to
/// the version-independent `opt/<formula>/bin` symlink.
pub fn service_program(formula: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locate the current binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    if let Some(stable) = homebrew_opt_bin(&exe, formula) {
        if stable.exists() {
            return Ok(stable);
        }
    }
    Ok(exe)
}

pub fn log_paths(log_name: &str) -> (PathBuf, PathBuf) {
    let dir = log_dir();
    (
        dir.join(format!("{log_name}.log")),
        dir.join(format!("{log_name}.err.log")),
    )
}

pub fn log_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Library/Logs/activity-watch-manager")
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::state_dir()
            .or_else(dirs::data_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("activity-watch-manager/logs")
    }
}

pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<String> = text.lines().map(str::to_string).collect();
    let start = all.len().saturating_sub(lines);
    all[start..].to_vec()
}

pub fn homebrew_opt_bin(exe: &Path, formula: &str) -> Option<PathBuf> {
    let components: Vec<_> = exe.components().collect();
    let cellar = components.iter().position(|c| c.as_os_str() == "Cellar")?;
    if cellar == 0 {
        return None;
    }
    let prefix: PathBuf = components[..cellar].iter().collect();
    let name = exe.file_name()?;
    Some(prefix.join("opt").join(formula).join("bin").join(name))
}

pub fn parse_launchctl_print(text: &str) -> ServiceState {
    let mut state = String::new();
    let mut pid = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("state =") {
            state = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("pid =") {
            pid = value.trim().parse().ok();
        }
    }
    let running = state == "running" && pid.is_some();
    ServiceState {
        installed: !state.is_empty(),
        running,
        pid,
        state: if state.is_empty() {
            "not installed".into()
        } else {
            state
        },
    }
}

#[cfg(target_os = "macos")]
fn install_launchd(spec: &ServiceSpec, out_log: &Path, err_log: &Path) -> Result<()> {
    let uid = user_id()?;
    let plist_dir = dirs::home_dir()
        .context("home directory")?
        .join("Library/LaunchAgents");
    fs::create_dir_all(&plist_dir)?;
    let plist = plist_dir.join(format!("{}.plist", spec.label));
    fs::write(&plist, render_plist(spec, out_log, err_log))
        .with_context(|| format!("write {}", plist.display()))?;

    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{}", spec.label);
    let _ = Command::new("launchctl")
        .args(["bootout", &target])
        .status();
    let boot = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist.to_string_lossy()])
        .status()
        .context("launchctl bootstrap")?;
    if !boot.success() {
        bail!("launchctl bootstrap failed for {}", spec.label);
    }
    let _ = Command::new("launchctl").args(["enable", &target]).status();
    let kick = Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status()
        .context("launchctl kickstart")?;
    if !kick.success() {
        bail!(
            "registered {}, but it did not start. Check {}",
            spec.label,
            out_log.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_launchd(label: &str) -> Result<()> {
    let uid = user_id()?;
    let target = format!("gui/{uid}/{label}");
    let _ = Command::new("launchctl")
        .args(["bootout", &target])
        .status();
    if let Some(home) = dirs::home_dir() {
        let plist = home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"));
        if plist.exists() {
            fs::remove_file(&plist).with_context(|| format!("remove {}", plist.display()))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn query_launchd(label: &str) -> ServiceState {
    let Ok(uid) = user_id() else {
        return missing();
    };
    let output = Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            parse_launchctl_print(&text)
        }
        _ => missing(),
    }
}

#[cfg(target_os = "linux")]
fn install_systemd(spec: &ServiceSpec, out_log: &Path, err_log: &Path) -> Result<()> {
    let dir = dirs::config_dir()
        .context("config directory")?
        .join("systemd/user");
    fs::create_dir_all(&dir)?;
    let unit_path = dir.join(format!("{}.service", spec.formula));
    fs::write(&unit_path, render_systemd_unit(spec, out_log, err_log))?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run(
        "systemctl",
        &[
            "--user",
            "enable",
            "--now",
            &format!("{}.service", spec.formula),
        ],
    )?;
    // Linger lets the user service start at boot, not only after a graphical login.
    let _ = Command::new("loginctl")
        .args(["enable-linger", &whoami()])
        .status();
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_systemd(label: &str) -> Result<()> {
    let formula = if label.contains("collector") {
        "awm-collector"
    } else {
        "awm-server"
    };
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", &format!("{formula}.service")])
        .status();
    if let Some(config) = dirs::config_dir() {
        let unit = config
            .join("systemd/user")
            .join(format!("{formula}.service"));
        if unit.exists() {
            fs::remove_file(unit)?;
        }
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = label;
    Ok(())
}

#[cfg(target_os = "linux")]
fn query_systemd(label: &str) -> ServiceState {
    let formula = if label.contains("collector") {
        "awm-collector"
    } else {
        "awm-server"
    };
    let unit = format!("{formula}.service");
    let show = Command::new("systemctl")
        .args([
            "--user",
            "show",
            &unit,
            "-p",
            "ActiveState",
            "-p",
            "MainPID",
            "-p",
            "UnitFileState",
            "-p",
            "LoadState",
        ])
        .output();
    let Ok(show) = show else {
        return missing();
    };
    if !show.status.success() {
        return missing();
    }
    let text = String::from_utf8_lossy(&show.stdout);
    let mut active = String::new();
    let mut file_state = String::new();
    let mut load = String::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("ActiveState=") {
            active = value.to_string();
        } else if let Some(value) = line.strip_prefix("UnitFileState=") {
            file_state = value.to_string();
        } else if let Some(value) = line.strip_prefix("LoadState=") {
            load = value.to_string();
        } else if let Some(value) = line.strip_prefix("MainPID=") {
            pid = value.parse().ok().filter(|pid: &u32| *pid > 0);
        }
    }
    if load == "not-found" {
        return missing();
    }
    let installed = file_state == "enabled" || active == "active" || load == "loaded";
    ServiceState {
        installed,
        running: active == "active",
        pid,
        state: if active.is_empty() {
            "not installed".into()
        } else {
            active
        },
    }
}

#[cfg(target_os = "linux")]
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| cmd.to_string())?;
    if !status.success() {
        bail!("{cmd} {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn whoami() -> String {
    Command::new("id")
        .arg("-un")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "user".into())
}

fn missing() -> ServiceState {
    ServiceState {
        installed: false,
        running: false,
        pid: None,
        state: "not installed".into(),
    }
}

#[cfg(target_os = "macos")]
fn user_id() -> Result<u32> {
    let output = Command::new("id").arg("-u").output().context("id -u")?;
    let text = String::from_utf8(output.stdout).context("id -u output")?;
    text.trim().parse().context("parse uid")
}

pub fn render_plist(spec: &ServiceSpec, out_log: &Path, err_log: &Path) -> String {
    let mut args = format!(
        "    <string>{}</string>\n",
        xml(&spec.program.to_string_lossy())
    );
    for arg in &spec.args {
        args.push_str(&format!("    <string>{}</string>\n", xml(arg)));
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>Nice</key>
  <integer>{nice}</integer>
  <key>WorkingDirectory</key>
  <string>{home}</string>
  <key>StandardOutPath</key>
  <string>{out_log}</string>
  <key>StandardErrorPath</key>
  <string>{err_log}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
</dict>
</plist>
"#,
        label = xml(&spec.label),
        nice = spec.nice,
        home = xml(&home.to_string_lossy()),
        out_log = xml(&out_log.to_string_lossy()),
        err_log = xml(&err_log.to_string_lossy()),
    )
}

#[cfg(target_os = "linux")]
fn render_systemd_unit(spec: &ServiceSpec, out_log: &Path, err_log: &Path) -> String {
    let mut exec = spec.program.to_string_lossy().into_owned();
    for arg in &spec.args {
        exec.push(' ');
        exec.push_str(&shell_quote(arg));
    }
    format!(
        r#"[Unit]
Description=Activity Watch Manager {formula}
After=network-online.target

[Service]
ExecStart={exec}
Restart=always
RestartSec=10
Nice={nice}
StandardOutput=append:{out_log}
StandardError=append:{err_log}
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#,
        formula = spec.formula,
        nice = spec.nice,
        out_log = out_log.display(),
        err_log = err_log.display(),
    )
}

#[cfg(target_os = "linux")]
fn shell_quote(arg: &str) -> String {
    if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "/._-".contains(ch))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homebrew_cellar_path_becomes_the_opt_symlink() {
        let exe = Path::new("/opt/homebrew/Cellar/awm-server/0.1.0/bin/awm-server");
        let stable = homebrew_opt_bin(exe, "awm-server").unwrap();
        assert_eq!(
            stable,
            PathBuf::from("/opt/homebrew/opt/awm-server/bin/awm-server")
        );
    }

    #[test]
    fn non_homebrew_path_is_left_alone() {
        assert!(
            homebrew_opt_bin(Path::new("/Users/me/.local/bin/awm-server"), "awm-server").is_none()
        );
    }

    #[test]
    fn launchctl_print_marks_a_running_service() {
        let state = parse_launchctl_print(
            "gui/501/com.activitywatch.manager.server = {\n\tstate = running\n\tpid = 4242\n}\n",
        );
        assert!(state.installed);
        assert!(state.running);
        assert_eq!(state.pid, Some(4242));
    }

    #[test]
    fn plist_starts_at_login_and_stays_up() {
        let spec = ServiceSpec::server(
            PathBuf::from("/opt/homebrew/opt/awm-server/bin/awm-server"),
            vec![],
        );
        let text = render_plist(
            &spec,
            Path::new("/tmp/server.log"),
            Path::new("/tmp/server.err.log"),
        );
        assert!(text.contains("<key>RunAtLoad</key>"));
        assert!(text.contains("<key>KeepAlive</key>"));
        assert!(text.contains("/opt/homebrew/opt/awm-server/bin/awm-server"));
        assert!(text.contains(SERVER_LABEL));
    }
}
