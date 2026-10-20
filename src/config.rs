//! Configuration from environment variables (a `.env` file is not read; use
//! systemd `EnvironmentFile=` or export them).

use std::{env, net::SocketAddr, path::PathBuf};

pub struct Config {
    pub bind: SocketAddr,
    /// Optional URL prefix the routes are also served under, e.g. "/claude/api".
    pub base_path: Option<String>,
    /// Static keys from .env (optional once keys are managed through the admin API).
    pub api_keys: Vec<String>,
    /// Enables the /admin endpoints when set.
    pub admin_key: Option<String>,
    /// JSON file holding keys created through the admin API.
    pub keys_file: PathBuf,
    pub claude_bin: String,
    pub workdir: PathBuf,
    /// Empty = plain chat. Otherwise the Claude Code tools the agent may use.
    pub tools: Vec<String>,
    pub max_turns: u32,
    pub default_model: String,
    pub models: Vec<String>,
    pub max_concurrent: usize,
    pub timeout_secs: u64,
    /// Used as the system prompt in chat mode when the request has none.
    pub default_system_file: PathBuf,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn list(name: &str) -> Vec<String> {
    var(name)
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match var(name) {
        Some(v) => v.parse().map_err(|_| format!("{name}: invalid value {v:?}")),
        None => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let api_keys = list("GATEWAY_API_KEYS");
        let admin_key = var("GATEWAY_ADMIN_KEY");
        if api_keys.is_empty() && admin_key.is_none() {
            return Err("set GATEWAY_ADMIN_KEY (to manage keys via /admin) and/or GATEWAY_API_KEYS".into());
        }
        if admin_key.as_ref().is_some_and(|k| k.len() < 24) {
            return Err("GATEWAY_ADMIN_KEY must be at least 24 characters".into());
        }

        let data_dir = var("GATEWAY_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share/claude-gateway")
        });
        let workdir = var("GATEWAY_WORKDIR").map(PathBuf::from).unwrap_or_else(|| data_dir.join("work"));
        for dir in [&data_dir, &workdir] {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let default_system_file = data_dir.join("default-system.txt");
        std::fs::write(&default_system_file, "You are Claude, a helpful AI assistant.")
            .map_err(|e| format!("write {}: {e}", default_system_file.display()))?;

        let default_model = var("GATEWAY_DEFAULT_MODEL").unwrap_or_else(|| "claude-opus-5-5".into());
        let mut models = list("GATEWAY_MODELS");
        if models.is_empty() {
            models = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"]
                .map(String::from)
                .to_vec();
        }

        Ok(Self {
            bind: parse("GATEWAY_BIND", "0.0.0.0:8443".parse().unwrap())?,
            base_path: var("GATEWAY_BASE_PATH")
                .map(|p| format!("/{}", p.trim_matches('/')))
                .filter(|p| p != "/"),
            api_keys,
            admin_key,
            keys_file: var("GATEWAY_KEYS_FILE").map(PathBuf::from).unwrap_or_else(|| data_dir.join("keys.json")),
            claude_bin: var("CLAUDE_BIN").unwrap_or_else(|| "claude".into()),
            workdir,
            tools: list("GATEWAY_TOOLS"),
            max_turns: parse("GATEWAY_MAX_TURNS", 20)?,
            default_model,
            models,
            max_concurrent: parse("GATEWAY_MAX_CONCURRENT", 4)?,
            timeout_secs: parse("GATEWAY_TIMEOUT_SECS", 600)?,
            default_system_file,
        })
    }
}
