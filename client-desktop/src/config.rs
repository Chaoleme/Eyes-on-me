use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};
use url::Url;
use uuid::Uuid;

pub const DESKTOP_AGENT_NAME: &str = "client-desktop";
const CONFIG_FILE_NAME: &str = "client-desktop.config.json";
const IDENTITY_FILE_NAME: &str = "client-desktop.identity.json";
const CURRENT_CONFIG_VERSION: u32 = 2;

#[cfg(target_os = "macos")]
const DEFAULT_DEVICE_ID: &str = "macos-agent";
#[cfg(target_os = "windows")]
const DEFAULT_DEVICE_ID: &str = "windows-agent";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const DEFAULT_DEVICE_ID: &str = "client-desktop";

#[derive(Debug, Clone)]
pub struct Config {
    pub server_api_base_url: String,
    pub device_id: String,
    pub agent_name: String,
    pub api_token: String,
    pub capture_filters: CaptureFilters,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureFilters {
    #[serde(default)]
    pub ignored_apps: Vec<String>,
    #[serde(default)]
    pub ignored_domains: Vec<String>,
}

impl CaptureFilters {
    fn normalize(&mut self) {
        self.ignored_apps = normalize_string_list(std::mem::take(&mut self.ignored_apps), false);
        self.ignored_domains =
            normalize_string_list(std::mem::take(&mut self.ignored_domains), true);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredConfig {
    #[serde(default = "current_config_version")]
    version: u32,
    #[serde(alias = "server_ws_url", default = "default_server_api_base_url")]
    server_api_base_url: String,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default = "default_agent_name")]
    agent_name: String,
    #[serde(default = "default_agent_api_token")]
    api_token: String,
    #[serde(default)]
    capture_filters: CaptureFilters,
    #[serde(default, skip_serializing)]
    ignored_apps: Vec<String>,
    #[serde(default, skip_serializing)]
    ignored_domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredIdentity {
    device_id: String,
    created_at: u64,
}

impl Config {
    pub fn from_prompt() -> Result<Self> {
        let config_path = resolve_config_path();
        let identity = ensure_stable_identity(&resolve_identity_path(&config_path))?;
        let stored_config = load_stored_config(&config_path, &identity);
        let no_prompt = env::var("AGENT_NO_PROMPT")
            .ok()
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);

        #[cfg(target_os = "windows")]
        {
            let stored_config =
                stored_config.unwrap_or_else(|| StoredConfig::new_default(&identity));
            save_stored_config(&config_path, &stored_config);
            return stored_config.into_runtime();
        }

        if no_prompt {
            let config = stored_config.unwrap_or_else(|| StoredConfig::new_default(&identity));
            save_stored_config(&config_path, &config);
            return config.into_runtime();
        }

        let stored_config = stored_config.unwrap_or_else(|| StoredConfig::new_default(&identity));
        let default_server_api_base_url = stored_config.server_api_base_url.clone();
        let server_api_base_url = prompt_server_api_base_url(&default_server_api_base_url)?;

        let default_device_id = stored_config
            .device_id
            .clone()
            .unwrap_or_else(|| identity.device_id.clone());
        let device_id = prompt_device_id(&default_device_id)?;

        let default_agent_name = stored_config.agent_name.clone();
        let agent_name = prompt_agent_name(&default_agent_name)?;

        let default_api_token = stored_config.api_token.clone();
        let api_token = prompt_agent_api_token(&default_api_token)?;

        let updated = StoredConfig {
            server_api_base_url: server_api_base_url.clone(),
            device_id: Some(device_id.clone()),
            agent_name: agent_name.clone(),
            api_token: api_token.clone(),
            ..stored_config
        };
        save_stored_config(&config_path, &updated);

        updated.into_runtime()
    }
}

impl StoredConfig {
    fn new_default(identity: &StoredIdentity) -> Self {
        let mut config = Self {
            version: current_config_version(),
            server_api_base_url: default_server_api_base_url(),
            device_id: Some(identity.device_id.clone()),
            agent_name: default_agent_name(),
            api_token: default_agent_api_token(),
            capture_filters: CaptureFilters::default(),
            ignored_apps: Vec::new(),
            ignored_domains: Vec::new(),
        };
        config.normalize(identity);
        config
    }

    fn normalize(&mut self, identity: &StoredIdentity) {
        self.server_api_base_url = self.server_api_base_url.trim().to_string();
        self.agent_name = normalize_required_text(self.agent_name.clone(), default_agent_name());
        self.api_token = normalize_required_text(self.api_token.clone(), default_agent_api_token());

        self.capture_filters
            .ignored_apps
            .append(&mut self.ignored_apps);
        self.capture_filters
            .ignored_domains
            .append(&mut self.ignored_domains);
        self.capture_filters.normalize();

        self.device_id = self
            .device_id
            .take()
            .and_then(normalize_optional_text)
            .or_else(|| Some(identity.device_id.clone()));

        self.version = current_config_version();
    }

    fn into_runtime(mut self) -> Result<Config> {
        let identity = StoredIdentity {
            device_id: self
                .device_id
                .clone()
                .unwrap_or_else(|| generated_device_id()),
            created_at: unix_timestamp_secs(),
        };
        self.normalize(&identity);

        Ok(Config {
            server_api_base_url: normalize_server_api_base_url(self.server_api_base_url)?,
            device_id: self
                .device_id
                .ok_or_else(|| anyhow!("device id is required"))?,
            agent_name: self.agent_name,
            api_token: self.api_token,
            capture_filters: self.capture_filters,
        })
    }
}

pub fn normalize_server_api_base_url(url: String) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/').to_string();

    if let Some(prefix) = trimmed.strip_suffix("/api/agent/activity") {
        return validate_server_api_base_url(prefix.to_string());
    }

    if let Some(prefix) = trimmed.strip_suffix("/api/agent/status") {
        return validate_server_api_base_url(prefix.to_string());
    }

    if let Some(prefix) = trimmed.strip_suffix("/ws/agent") {
        let corrected = to_http_base(prefix);
        warn!(
            original = %trimmed,
            corrected = %corrected,
            "agent backend points to websocket endpoint; auto-corrected to HTTP base URL"
        );
        return validate_server_api_base_url(corrected);
    }

    if let Some(prefix) = trimmed.strip_suffix("/ws/dashboard") {
        let corrected = to_http_base(prefix);
        warn!(
            original = %trimmed,
            corrected = %corrected,
            "agent backend points to dashboard websocket; auto-corrected to HTTP base URL"
        );
        return validate_server_api_base_url(corrected);
    }

    if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        return validate_server_api_base_url(to_http_base(&trimmed));
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return validate_server_api_base_url(trimmed);
    }

    validate_server_api_base_url(format!("http://{trimmed}"))
}

fn current_config_version() -> u32 {
    CURRENT_CONFIG_VERSION
}

fn to_http_base(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("ws://") {
        return format!("http://{rest}");
    }

    if let Some(rest) = value.strip_prefix("wss://") {
        return format!("https://{rest}");
    }

    value.to_string()
}

fn validate_server_api_base_url(url: String) -> Result<String> {
    let mut parsed = Url::parse(&url).map_err(|err| anyhow!("invalid backend url: {err}"))?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(anyhow!("agent backend must use http or https"));
    }

    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(anyhow!(
            "agent backend base url must not include query or fragment"
        ));
    }

    let normalized_path = parsed.path().trim_end_matches('/');
    if normalized_path == "/api" {
        return Err(anyhow!(
            "agent backend must point to service root, not /api"
        ));
    }

    if normalized_path.is_empty() || normalized_path == "/" {
        parsed.set_path("");
        return Ok(parsed.to_string().trim_end_matches('/').to_string());
    }

    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

fn resolve_config_path() -> PathBuf {
    if let Ok(path) = env::var("AGENT_CONFIG_PATH") {
        let candidate = PathBuf::from(path);
        if candidate.file_name().is_some() {
            return candidate;
        }
    }

    let executable_dir_path = executable_dir_config_path();
    if executable_dir_path.is_file() {
        return executable_dir_path;
    }

    let current_dir_path = current_dir_config_path();
    if current_dir_path.is_file() {
        return current_dir_path;
    }

    executable_dir_path
}

fn resolve_identity_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(IDENTITY_FILE_NAME)
}

fn current_dir_config_path() -> PathBuf {
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(CONFIG_FILE_NAME)
}

fn executable_dir_config_path() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(CONFIG_FILE_NAME)
}

fn load_stored_config(path: &Path, identity: &StoredIdentity) -> Option<StoredConfig> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(path = %path.display(), %err, "failed to read config file");
            return None;
        }
    };

    match serde_json::from_str::<StoredConfig>(&raw) {
        Ok(mut config) => {
            config.normalize(identity);
            Some(config)
        }
        Err(err) => {
            warn!(path = %path.display(), %err, "failed to parse config file");
            None
        }
    }
}

fn save_stored_config(path: &Path, config: &StoredConfig) {
    let raw = match serde_json::to_string_pretty(config) {
        Ok(raw) => raw,
        Err(err) => {
            error!(path = %path.display(), %err, "failed to serialize config file");
            return;
        }
    };

    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            error!(path = %path.display(), %err, "failed to create config directory");
            return;
        }
    }

    if let Err(err) = write_atomic(path, &format!("{raw}\n")) {
        error!(path = %path.display(), %err, "failed to write config file");
    }
}

fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, content)?;
    fs::rename(&temp_path, path)?;
    Ok(())
}

fn ensure_stable_identity(path: &Path) -> Result<StoredIdentity> {
    match read_stable_identity(path) {
        Ok(Some(identity)) => return Ok(identity),
        Ok(None) => {}
        Err(err) => {
            warn!(path = %path.display(), %err, "failed to read identity file; regenerating")
        }
    }

    let identity = StoredIdentity {
        device_id: generated_device_id(),
        created_at: unix_timestamp_secs(),
    };

    write_stable_identity(path, &identity)
        .with_context(|| format!("failed to persist desktop identity at {}", path.display()))?;
    Ok(identity)
}

fn read_stable_identity(path: &Path) -> io::Result<Option<StoredIdentity>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    let identity = serde_json::from_str::<StoredIdentity>(&raw)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if normalize_optional_text(Some(identity.device_id.clone())).is_none() {
        return Ok(None);
    }

    Ok(Some(identity))
}

fn write_stable_identity(path: &Path, identity: &StoredIdentity) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let raw = serde_json::to_string_pretty(identity).map_err(io::Error::other)?;
    write_atomic(path, &format!("{raw}\n"))
}

fn default_server_api_base_url() -> String {
    env::var("AGENT_SERVER_API_BASE_URL")
        .or_else(|_| env::var("AGENT_SERVER_WS_URL"))
        .unwrap_or_else(|_| "http://127.0.0.1:8787".to_string())
}

fn generated_device_id() -> String {
    let base = hostname::get()
        .ok()
        .and_then(|host| host.into_string().ok())
        .and_then(normalize_optional_text)
        .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string());
    let base = slugify_device_id(&base);
    let suffix = Uuid::new_v4().simple().to_string()[..8].to_string();
    format!("{base}-{suffix}")
}

fn slugify_device_id(value: &str) -> String {
    let mut slug = String::with_capacity(value.len());
    let mut last_was_dash = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        DEFAULT_DEVICE_ID.to_string()
    } else {
        slug.to_string()
    }
}

fn prompt_server_api_base_url(default_value: &str) -> io::Result<String> {
    let mut stdout = io::stdout();
    writeln!(
        stdout,
        "Please enter backend address (example: http://127.0.0.1:8787)"
    )?;
    write!(stdout, "Backend address [{default_value}]: ")?;
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let raw = if input.trim().is_empty() {
        default_value.to_string()
    } else {
        input
    };

    normalize_server_api_base_url(raw).map_err(io::Error::other)
}

fn prompt_device_id(default_value: &str) -> io::Result<String> {
    let mut stdout = io::stdout();
    writeln!(stdout, "Please enter current device ID")?;
    write!(stdout, "Device ID [{default_value}]: ")?;
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(default_value.to_string());
    }

    Ok(trimmed.to_string())
}

fn default_agent_api_token() -> String {
    env::var("AGENT_API_TOKEN").unwrap_or_else(|_| "dev-agent-token".to_string())
}

fn default_agent_name() -> String {
    env::var("AGENT_NAME").unwrap_or_else(|_| DESKTOP_AGENT_NAME.to_string())
}

fn prompt_agent_name(default_value: &str) -> io::Result<String> {
    let mut stdout = io::stdout();
    writeln!(stdout, "Please enter current agent name")?;
    write!(stdout, "Agent name [{default_value}]: ")?;
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let trimmed = input.trim();
    if trimmed.is_empty() {
        if default_value.trim().is_empty() {
            return Err(io::Error::other("agent name is required"));
        }

        return Ok(default_value.to_string());
    }

    Ok(trimmed.to_string())
}

fn prompt_agent_api_token(default_value: &str) -> io::Result<String> {
    let mut stdout = io::stdout();
    writeln!(stdout, "Please enter agent API token")?;
    write!(stdout, "Agent API token [{default_value}]: ")?;
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let trimmed = input.trim();
    if trimmed.is_empty() {
        if default_value.trim().is_empty() {
            return Err(io::Error::other("agent API token is required"));
        }

        return Ok(default_value.to_string());
    }

    Ok(trimmed.to_string())
}

fn normalize_optional_text<T>(value: T) -> Option<String>
where
    T: Into<Option<String>>,
{
    value.into().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_required_text(value: String, fallback: String) -> String {
    normalize_optional_text(Some(value)).unwrap_or(fallback)
}

fn normalize_string_list(values: Vec<String>, lowercase: bool) -> Vec<String> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();

    for value in values {
        let Some(mut candidate) = normalize_optional_text(Some(value)) else {
            continue;
        };
        if lowercase {
            candidate = candidate.to_lowercase();
        }
        let key = candidate.to_lowercase();
        if seen.insert(key) {
            normalized.push(candidate);
        }
    }

    normalized
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        CaptureFilters, StoredConfig, StoredIdentity, current_config_version, default_agent_name,
        ensure_stable_identity, load_stored_config, normalize_server_api_base_url,
        resolve_identity_path, save_stored_config, validate_server_api_base_url,
    };

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("valid time")
            .as_nanos();
        std::env::temp_dir().join(format!("eyes-on-me-{label}-{unique}"))
    }

    #[test]
    fn allows_http_base_urls() {
        let result = validate_server_api_base_url("http://example.com:8787".to_string());
        assert!(result.is_ok());
    }

    #[test]
    fn converts_websocket_urls() {
        let result = normalize_server_api_base_url("ws://127.0.0.1:8787/ws/agent".to_string());
        assert_eq!(result.unwrap(), "http://127.0.0.1:8787");
    }

    #[test]
    fn rejects_api_paths() {
        let result = validate_server_api_base_url("http://127.0.0.1:8787/api".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn writes_config_atomically() {
        let dir = temp_dir("config");
        let path = dir.join("client-desktop.config.json");

        save_stored_config(
            &path,
            &StoredConfig {
                version: current_config_version(),
                server_api_base_url: "http://127.0.0.1:8787".to_string(),
                device_id: Some("device-1".to_string()),
                agent_name: default_agent_name(),
                api_token: "token-1".to_string(),
                capture_filters: CaptureFilters::default(),
                ignored_apps: Vec::new(),
                ignored_domains: Vec::new(),
            },
        );

        let content = fs::read_to_string(&path).expect("config file should exist");
        assert!(content.contains("\"device_id\": \"device-1\""));
        assert!(content.contains(&format!("\"version\": {}", current_config_version())));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_stable_identity_across_reads() {
        let dir = temp_dir("identity");
        let config_path = dir.join("client-desktop.config.json");
        let identity_path = resolve_identity_path(&config_path);

        let first = ensure_stable_identity(&identity_path).expect("should create identity");
        let second = ensure_stable_identity(&identity_path).expect("should reuse identity");

        assert_eq!(first.device_id, second.device_id);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_legacy_config_into_capture_filters() {
        let dir = temp_dir("legacy-config");
        let config_path = dir.join("client-desktop.config.json");
        fs::create_dir_all(&dir).expect("create temp dir");
        fs::write(
            &config_path,
            r#"{
  "server_api_base_url": "http://127.0.0.1:8787",
  "device_id": "legacy-device",
  "agent_name": "client-desktop",
  "api_token": "token-1",
  "ignored_apps": ["WeChat", " WeChat "],
  "ignored_domains": ["github.com", "GitHub.com"]
}"#,
        )
        .expect("write legacy config");

        let identity = StoredIdentity {
            device_id: "generated-device".to_string(),
            created_at: 1,
        };
        let config = load_stored_config(&config_path, &identity).expect("load config");

        assert_eq!(config.version, current_config_version());
        assert_eq!(config.device_id.as_deref(), Some("legacy-device"));
        assert_eq!(config.capture_filters.ignored_apps, vec!["WeChat"]);
        assert_eq!(config.capture_filters.ignored_domains, vec!["github.com"]);

        let _ = fs::remove_dir_all(&dir);
    }
}
