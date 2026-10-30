use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelAliasesFile {
    #[serde(default)]
    pub subscription: Option<Vec<String>>,
    #[serde(default)]
    pub credits: Option<Vec<String>>,
    #[serde(default, rename = "default_kind")]
    pub default_kind: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FileConfig {
    pub bind: Option<String>,
    pub proxy_key: Option<String>,
    pub ui_token: Option<String>,
    pub codex_cooldown_secs: Option<u64>,
    pub copilot_cooldown_secs: Option<u64>,
    pub cline_cooldown_secs: Option<u64>,
    pub cline_cooldown_ms: Option<u64>,
    pub opencode_go_cooldown_secs: Option<u64>,
    pub opencode_go_cooldown_ms: Option<u64>,
    /// Outbound user agent for the fork's own backends. `user_agent` is the shared default and
    /// the per-backend keys override it.
    pub user_agent: Option<String>,
    pub cline_user_agent: Option<String>,
    pub opencode_go_user_agent: Option<String>,
    pub claude_cooldown_secs: Option<u64>,
    pub claude_cooldown_ms: Option<u64>,
    pub copilot_cooldown_ms: Option<u64>,
    pub codex_cooldown_ms: Option<u64>,
    pub auto_codex_resets: Option<bool>,
    pub stats_retention_days: Option<i64>,
    #[serde(default)]
    pub model_aliases: Option<ModelAliasesFile>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub proxy_key: Option<String>,
    pub ui_token: Option<String>,
    pub codex_cooldown_ms: i64,
    pub auto_codex_resets: bool,
    pub copilot_cooldown_ms: i64,
    pub cline_cooldown_ms: i64,
    pub opencode_go_cooldown_ms: i64,
    pub claude_cooldown_ms: i64,
    /// The outbound user agent for Cline and OpenCode Go, already resolved through the per-backend
    /// keys. `None` leaves each backend its default and forwards a client's own value.
    pub cline_user_agent: Option<String>,
    pub opencode_go_user_agent: Option<String>,
    pub stats_retention_days: i64,
    pub model_alias_kinds: BTreeMap<String, Vec<String>>,
    pub model_alias_default_kind: String,
    pub data_dir: PathBuf,
    #[allow(dead_code)]
    pub config_dir: PathBuf,
}

pub const DEFAULT_COOLDOWN_MS: i64 = 30 * 60 * 1000;
pub const SYSTEM_MONITOR_SOCKET: &str = "/run/underclass/monitor.sock";

/// The first configured value among `candidates`, in precedence order. A blank or whitespace-only
/// value is not a configured identity: an environment variable exported empty (a service unit that
/// sets `UNDERCLASS_USER_AGENT=`) MUST NOT shadow a value set in the config file, and the resolver
/// would ignore the blank value anyway.
fn first_configured(candidates: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    candidates
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn default_data_dir() -> PathBuf {
    home_dir().join(".local/share/underclass")
}

pub fn default_config_dir() -> PathBuf {
    home_dir().join(".config/underclass")
}

impl Config {
    pub fn load() -> Self {
        let config_dir = std::env::var_os("UNDERCLASS_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_config_dir);
        let data_dir = std::env::var_os("UNDERCLASS_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_data_dir);

        let mut file_cfg = FileConfig::default();
        let config_path = config_dir.join("config.toml");
        if let Ok(text) = std::fs::read_to_string(&config_path) {
            if let Ok(parsed) = toml::from_str::<FileConfig>(&text) {
                file_cfg = parsed;
            } else {
                eprintln!(
                    "warning: could not parse {}, using defaults",
                    config_path.display()
                );
            }
        }

        let bind = std::env::var("UNDERCLASS_BIND")
            .ok()
            .or(file_cfg.bind)
            .unwrap_or_else(|| "127.0.0.1:8080".to_string());
        let proxy_key = std::env::var("UNDERCLASS_PROXY_KEY")
            .ok()
            .or(file_cfg.proxy_key);
        let ui_token = std::env::var("UNDERCLASS_UI_TOKEN")
            .ok()
            .or(file_cfg.ui_token);
        let codex_cooldown_ms = file_cfg
            .codex_cooldown_ms
            .or(file_cfg.codex_cooldown_secs.map(|s| s * 1000))
            .unwrap_or(DEFAULT_COOLDOWN_MS as u64) as i64;
        let copilot_cooldown_ms = file_cfg
            .copilot_cooldown_ms
            .or(file_cfg.copilot_cooldown_secs.map(|s| s * 1000))
            .unwrap_or(DEFAULT_COOLDOWN_MS as u64) as i64;
        let cline_cooldown_ms = std::env::var("UNDERCLASS_CLINE_COOLDOWN_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1000))
            .or(file_cfg.cline_cooldown_ms)
            .or(file_cfg.cline_cooldown_secs.map(|s| s * 1000))
            .unwrap_or(DEFAULT_COOLDOWN_MS as u64) as i64;
        let opencode_go_cooldown_ms = std::env::var("UNDERCLASS_OPENCODE_GO_COOLDOWN_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1000))
            .or(file_cfg.opencode_go_cooldown_ms)
            .or(file_cfg.opencode_go_cooldown_secs.map(|s| s * 1000))
            .unwrap_or(DEFAULT_COOLDOWN_MS as u64) as i64;
        let claude_cooldown_ms = file_cfg
            .claude_cooldown_ms
            .or(file_cfg.claude_cooldown_secs.map(|s| s * 1000))
            .unwrap_or(DEFAULT_COOLDOWN_MS as u64) as i64;

        let auto_codex_resets = std::env::var("UNDERCLASS_AUTO_CODEX_RESETS")
            .ok()
            .and_then(|value| value.parse::<bool>().ok())
            .or(file_cfg.auto_codex_resets)
            .unwrap_or(true);

        let stats_retention_days = std::env::var("UNDERCLASS_STATS_RETENTION_DAYS")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .or(file_cfg.stats_retention_days)
            .unwrap_or(crate::stats::DEFAULT_RETENTION_DAYS)
            .max(1);

        // Model alias classification config.
        // Start from defaults, then let the operator override/extend per kind. A partial
        // config therefore never silently drops the default classification of other kinds.
        let mut model_alias_kinds = crate::aliases::AliasConfig::default_config().kinds;
        if let Some(ma) = &file_cfg.model_aliases {
            if let Some(v) = &ma.subscription {
                model_alias_kinds.insert("subscription".to_string(), v.clone());
            }
            if let Some(v) = &ma.credits {
                model_alias_kinds.insert("credits".to_string(), v.clone());
            }
            for (k, v) in &ma.extra {
                model_alias_kinds.insert(k.clone(), v.clone());
            }
        }
        let mut model_alias_default_kind = file_cfg
            .model_aliases
            .as_ref()
            .and_then(|m| m.default_kind.clone())
            .unwrap_or_else(|| "subscription".to_string());
        // Validate against configured kinds (and the built-in defaults).
        if !model_alias_kinds.contains_key(&model_alias_default_kind) {
            eprintln!(
                "warning: model_aliases.default_kind {:?} is not a configured kind; falling back to 'subscription'",
                model_alias_default_kind
            );
            // Preserve operator-configured kinds; only fix the default.
            if model_alias_kinds.contains_key("subscription") {
                model_alias_default_kind = "subscription".to_string();
            } else if let Some(first) = model_alias_kinds.keys().next().cloned() {
                model_alias_default_kind = first;
            } else {
                model_alias_kinds = crate::aliases::AliasConfig::default_config().kinds;
                model_alias_default_kind = "subscription".to_string();
            }
        }

        // `user_agent` is the shared default, so a per-backend key that is absent falls back to it
        // rather than to the built-in default.
        let shared_user_agent = first_configured([
            std::env::var("UNDERCLASS_USER_AGENT").ok(),
            file_cfg.user_agent,
        ]);
        let cline_user_agent = first_configured([
            std::env::var("UNDERCLASS_CLINE_USER_AGENT").ok(),
            file_cfg.cline_user_agent,
            shared_user_agent.clone(),
        ]);
        let opencode_go_user_agent = first_configured([
            std::env::var("UNDERCLASS_OPENCODE_GO_USER_AGENT").ok(),
            file_cfg.opencode_go_user_agent,
            shared_user_agent.clone(),
        ]);

        Self {
            bind,
            proxy_key,
            ui_token,
            codex_cooldown_ms,
            auto_codex_resets,
            copilot_cooldown_ms,
            cline_cooldown_ms,
            opencode_go_cooldown_ms,
            claude_cooldown_ms,
            cline_user_agent,
            opencode_go_user_agent,
            stats_retention_days,
            model_alias_kinds,
            model_alias_default_kind,
            data_dir,
            config_dir,
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("pool.db")
    }

    /// @cc [owner:ghuntley,label:cli] monitor-socket-location
    /// The server MUST honor a nonempty `UNDERCLASS_MONITOR_SOCKET` and otherwise place its local
    /// monitor socket beside the SQLite database.
    pub fn monitor_socket_path(&self) -> PathBuf {
        std::env::var_os("UNDERCLASS_MONITOR_SOCKET")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.data_dir.join("monitor.sock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment variable that is exported but empty must not shadow a configured identity:
    /// a service unit that sets `UNDERCLASS_USER_AGENT=` would otherwise silently discard the
    /// `user_agent` in the file.
    #[test]
    fn a_blank_user_agent_does_not_shadow_a_configured_one() {
        assert_eq!(
            first_configured([Some(String::new()), Some("from-file/1".into())]).as_deref(),
            Some("from-file/1")
        );
        assert_eq!(
            first_configured([Some("   ".into()), Some("from-file/1".into())]).as_deref(),
            Some("from-file/1")
        );
        // A real value still wins over the file, in precedence order.
        assert_eq!(
            first_configured([Some("from-env/2".into()), Some("from-file/1".into())]).as_deref(),
            Some("from-env/2")
        );
        assert_eq!(first_configured([Some(String::new()), None]), None);
    }
}
