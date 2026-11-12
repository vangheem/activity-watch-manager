use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use awm_proto::CategoryRule;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub token: String,
    #[serde(default)]
    pub data_dir: String,
    #[serde(default = "default_period_days")]
    pub period_days: i64,
    #[serde(default = "default_true")]
    pub trust_localhost: bool,
    #[serde(default)]
    pub timezone: String,
    #[serde(default)]
    pub categories: Vec<CategoryRule>,
}

fn default_listen() -> String {
    "0.0.0.0:8787".into()
}

fn default_period_days() -> i64 {
    30
}

fn default_true() -> bool {
    true
}

pub fn config_path() -> PathBuf {
    app_config_dir().join("server.toml")
}

pub fn default_data_dir() -> PathBuf {
    app_data_dir().join("data")
}

pub fn app_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("activity-watch-manager")
}

pub fn app_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("activity-watch-manager")
}

pub fn ensure_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        restrict_dir(parent);
    }
    let token = generate_token()?;
    let text = render_config(&token);
    fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    restrict_file(path);
    Ok(true)
}

pub fn load_config(path: &Path) -> Result<ServerConfig> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let mut cfg: ServerConfig =
        toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
    cfg.token = cfg.token.trim().to_string();
    if cfg.token.is_empty() {
        bail!("token in {} is empty", path.display());
    }
    if cfg.data_dir.trim().is_empty() {
        cfg.data_dir = default_data_dir().to_string_lossy().into_owned();
    }
    if cfg.categories.is_empty() {
        cfg.categories = default_categories();
    }
    if !(1..=365).contains(&cfg.period_days) {
        bail!("period_days must be between 1 and 365");
    }
    Ok(cfg)
}

pub fn default_categories() -> Vec<CategoryRule> {
    vec![
        rule(
            "Code",
            "#c45c26",
            &[
                "Code",
                "Visual Studio Code",
                "Cursor",
                "Zed",
                "Windsurf",
                "Xcode",
                "Terminal",
                "iTerm2",
                "Warp",
                "ghostty",
                "Ghostty",
                "Alacritty",
                "WezTerm",
                "kitty",
                "IntelliJ IDEA",
                "WebStorm",
                "PyCharm",
                "RustRover",
                "Sublime Text",
                "Nova",
                "MacVim",
                "Neovide",
                "Tower",
                "Fork",
                "GitHub Desktop",
            ],
        ),
        rule(
            "Browser",
            "#3d6b8c",
            &[
                "Google Chrome",
                "Chrome",
                "Safari",
                "Firefox",
                "Arc",
                "Brave Browser",
                "Microsoft Edge",
                "Orion",
                "Dia",
                "Chromium",
                "Vivaldi",
                "Helium",
            ],
        ),
        rule(
            "Communication",
            "#6d4c7d",
            &[
                "Slack",
                "Messages",
                "Discord",
                "Zoom",
                "zoom.us",
                "Telegram",
                "Mail",
                "Microsoft Outlook",
                "Microsoft Teams",
                "Signal",
                "WhatsApp",
                "FaceTime",
                "Loom",
            ],
        ),
        rule(
            "Design",
            "#8a5a3b",
            &[
                "Figma",
                "Sketch",
                "Adobe Photoshop",
                "Pixelmator Pro",
                "Affinity Designer 2",
                "Affinity Photo 2",
            ],
        ),
        rule(
            "Media",
            "#2f6b4e",
            &[
                "Spotify",
                "Music",
                "IINA",
                "VLC",
                "Podcasts",
                "QuickTime Player",
            ],
        ),
    ]
}

fn rule(name: &str, color: &str, apps: &[&str]) -> CategoryRule {
    CategoryRule {
        name: name.into(),
        color: color.into(),
        apps: apps.iter().map(|app| (*app).to_string()).collect(),
    }
}

fn render_config(token: &str) -> String {
    let mut out = format!(
        r#"# activity-watch-manager server
# The token protects ingest and the UI from other machines.
# Localhost can open the UI without it while trust_localhost is true.

listen = "0.0.0.0:8787"
token = {token}
# Empty uses the default data directory (Application Support on macOS).
data_dir = ""
period_days = 30
trust_localhost = true
# Empty uses this machine's timezone. The UI also sends the browser timezone.
timezone = ""

# App groups for the UI. Delete every [[categories]] block to restore built-ins.
"#,
        token = toml_string(token),
    );
    for category in default_categories() {
        out.push_str("[[categories]]\n");
        out.push_str(&format!("name = {}\n", toml_string(&category.name)));
        out.push_str(&format!("color = {}\n", toml_string(&category.color)));
        out.push_str("apps = [");
        for (i, app) in category.apps.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&toml_string(app));
        }
        out.push_str("]\n\n");
    }
    out
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 24];
    let mut file = fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    std::io::Read::read_exact(&mut file, &mut bytes).context("read /dev/urandom")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
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
