use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct ModelAliasesFile {
    #[serde(default)]
    pub subscriptions: Option<Vec<String>>,
    /// Legacy key name (pre-rename). Accepted for backward compatibility.
    #[serde(default)]
    pub subscription: Option<Vec<String>>,
    #[serde(default)]
    pub credits: Option<Vec<String>>,
    #[serde(default, rename = "default_kind")]
    pub default_kind: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
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
    pub vercel_cooldown_secs: Option<u64>,
    pub vercel_cooldown_ms: Option<u64>,
    pub vercel_user_agent: Option<String>,
    pub claude_cooldown_secs: Option<u64>,
    pub claude_cooldown_ms: Option<u64>,
    pub copilot_cooldown_ms: Option<u64>,
    pub codex_cooldown_ms: Option<u64>,
    pub auto_codex_resets: Option<bool>,
    pub stats_retention_days: Option<i64>,
    /// How long a streamed response may produce nothing at all before it is ended with a terminal
    /// error event. `0` (the default) never ends a stream for being quiet.
    pub stream_idle_timeout_secs: Option<u64>,
    /// How long the upstream TCP/TLS connect may take. Bounds startup of an attempt only.
    pub upstream_connect_timeout_secs: Option<u64>,
    /// How long a stream's first output is held back while failover is still possible. Expiry
    /// only stops holding; it is never a deadline on the response.
    pub pre_output_hold_secs: Option<u64>,
    /// How many bytes may be buffered during that hold.
    pub pre_output_hold_bytes: Option<usize>,
    #[serde(default)]
    pub model_aliases: Option<ModelAliasesFile>,
}

/// The listen address when neither the environment nor the file names one.
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// Whether an automatic Codex reset redemption is attempted by default.
pub const DEFAULT_AUTO_CODEX_RESETS: bool = true;
/// The cooldown every backend falls back to when the upstream says nothing about a window.
pub const DEFAULT_COOLDOWN_MS: i64 = 30 * 60 * 1000;
/// How long the upstream TCP/TLS connect may take before the attempt fails over.
pub const DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS: u64 = 10;
/// How long a stream's first output is held back while failover is still possible.
pub const DEFAULT_PRE_OUTPUT_HOLD_SECS: u64 = crate::proxy::PRE_OUTPUT_HOLD.as_secs();
/// How many bytes are buffered during that hold.
pub const DEFAULT_PRE_OUTPUT_HOLD_BYTES: usize = crate::proxy::PRE_OUTPUT_HOLD_BYTES;

impl FileConfig {
    /// Every setting's built-in default, in the shape a `config.toml` takes. This is the single
    /// source the resolver below falls back to, and `config.example.toml` must parse to exactly
    /// this value (asserted in `tests`), so the shipped example cannot drift from the code.
    ///
    /// A blank value means "unset" everywhere the resolver reads one: `proxy_key` and `ui_token`
    /// are minted on first run, and a blank `user_agent` forwards the client's own. Those keys are
    /// therefore present as empty strings rather than as `None`, so the shipped example can show
    /// every key without changing behaviour. The `*_cooldown_ms` keys stay `None` because their
    /// `*_cooldown_secs` siblings carry the default.
    pub fn defaults() -> Self {
        let aliases = crate::aliases::AliasConfig::default_config();
        Self {
            bind: Some(DEFAULT_BIND.to_string()),
            proxy_key: Some(String::new()),
            ui_token: Some(String::new()),
            codex_cooldown_secs: Some((DEFAULT_COOLDOWN_MS / 1000) as u64),
            codex_cooldown_ms: None,
            copilot_cooldown_secs: Some((DEFAULT_COOLDOWN_MS / 1000) as u64),
            copilot_cooldown_ms: None,
            claude_cooldown_secs: Some((DEFAULT_COOLDOWN_MS / 1000) as u64),
            claude_cooldown_ms: None,
            cline_cooldown_secs: Some((DEFAULT_COOLDOWN_MS / 1000) as u64),
            cline_cooldown_ms: None,
            opencode_go_cooldown_secs: Some((DEFAULT_COOLDOWN_MS / 1000) as u64),
            opencode_go_cooldown_ms: None,
            vercel_cooldown_secs: Some((DEFAULT_COOLDOWN_MS / 1000) as u64),
            vercel_cooldown_ms: None,
            user_agent: Some(String::new()),
            cline_user_agent: Some(String::new()),
            opencode_go_user_agent: Some(String::new()),
            vercel_user_agent: Some(String::new()),
            auto_codex_resets: Some(DEFAULT_AUTO_CODEX_RESETS),
            stats_retention_days: Some(crate::stats::DEFAULT_RETENTION_DAYS),
            stream_idle_timeout_secs: Some(0),
            upstream_connect_timeout_secs: Some(DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS),
            pre_output_hold_secs: Some(DEFAULT_PRE_OUTPUT_HOLD_SECS),
            pre_output_hold_bytes: Some(DEFAULT_PRE_OUTPUT_HOLD_BYTES),
            model_aliases: Some(ModelAliasesFile {
                subscriptions: Some(
                    aliases
                        .kinds
                        .get("subscriptions")
                        .cloned()
                        .unwrap_or_default(),
                ),
                subscription: None,
                credits: Some(aliases.kinds.get("credits").cloned().unwrap_or_default()),
                default_kind: Some(aliases.default_kind),
                extra: Default::default(),
            }),
        }
    }
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
    pub vercel_cooldown_ms: i64,
    pub claude_cooldown_ms: i64,
    /// The outbound user agent for Cline and OpenCode Go, already resolved through the per-backend
    /// keys. `None` leaves each backend its default and forwards a client's own value.
    pub cline_user_agent: Option<String>,
    pub opencode_go_user_agent: Option<String>,
    pub vercel_user_agent: Option<String>,
    pub stats_retention_days: i64,
    /// `None` means no idle deadline at all: a long review may produce nothing between tokens for
    /// as long as the upstream takes.
    pub stream_idle_timeout: Option<std::time::Duration>,
    /// How long the upstream TCP/TLS connect may take before the attempt fails over.
    pub upstream_connect_timeout: std::time::Duration,
    /// How long a stream's first output is held back while failover is still possible, and how
    /// many bytes may be buffered meanwhile. Neither bounds the response itself.
    pub pre_output_hold: std::time::Duration,
    pub pre_output_hold_bytes: usize,
    pub model_alias_kinds: BTreeMap<String, Vec<String>>,
    pub model_alias_default_kind: String,
    pub data_dir: PathBuf,
    #[allow(dead_code)]
    pub config_dir: PathBuf,
}

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

/// Resolves the stream idle deadline from an environment value and a file value, in that order of
/// precedence. `0` means "no idle deadline" at either level, so an operator can turn it off over
/// the environment even when the file sets one - and the default is off, because a long review may
/// produce nothing between tokens for as long as the upstream takes.
fn resolve_idle_timeout(
    env_secs: Option<u64>,
    file_secs: Option<u64>,
) -> Option<std::time::Duration> {
    match env_secs.or(file_secs) {
        Some(secs) if secs > 0 => Some(std::time::Duration::from_secs(secs)),
        _ => None,
    }
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
            .unwrap_or_else(|| DEFAULT_BIND.to_string());
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
        let vercel_cooldown_ms = std::env::var("UNDERCLASS_VERCEL_COOLDOWN_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1000))
            .or(file_cfg.vercel_cooldown_ms)
            .or(file_cfg.vercel_cooldown_secs.map(|s| s * 1000))
            .unwrap_or(DEFAULT_COOLDOWN_MS as u64) as i64;
        let claude_cooldown_ms = file_cfg
            .claude_cooldown_ms
            .or(file_cfg.claude_cooldown_secs.map(|s| s * 1000))
            .unwrap_or(DEFAULT_COOLDOWN_MS as u64) as i64;

        let auto_codex_resets = std::env::var("UNDERCLASS_AUTO_CODEX_RESETS")
            .ok()
            .and_then(|value| value.parse::<bool>().ok())
            .or(file_cfg.auto_codex_resets)
            .unwrap_or(DEFAULT_AUTO_CODEX_RESETS);

        let stream_idle_timeout = resolve_idle_timeout(
            std::env::var("UNDERCLASS_STREAM_IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok()),
            file_cfg.stream_idle_timeout_secs,
        );
        let upstream_connect_timeout = std::env::var("UNDERCLASS_UPSTREAM_CONNECT_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .or(file_cfg.upstream_connect_timeout_secs)
            .unwrap_or(DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS);
        let pre_output_hold_secs = std::env::var("UNDERCLASS_PRE_OUTPUT_HOLD_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .or(file_cfg.pre_output_hold_secs)
            .unwrap_or(DEFAULT_PRE_OUTPUT_HOLD_SECS);
        let pre_output_hold_bytes = std::env::var("UNDERCLASS_PRE_OUTPUT_HOLD_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .or(file_cfg.pre_output_hold_bytes)
            .unwrap_or(DEFAULT_PRE_OUTPUT_HOLD_BYTES);
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
            if let Some(v) = &ma.subscriptions {
                model_alias_kinds.insert("subscriptions".to_string(), v.clone());
            } else if let Some(v) = &ma.subscription {
                model_alias_kinds.insert("subscriptions".to_string(), v.clone());
                eprintln!("warning: model_aliases.subscription is deprecated; use subscriptions");
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
            .unwrap_or_else(|| "subscriptions".to_string());
        // Validate against configured kinds (and the built-in defaults).
        if !model_alias_kinds.contains_key(&model_alias_default_kind) {
            eprintln!(
                "warning: model_aliases.default_kind {:?} is not a configured kind; falling back to 'subscriptions'",
                model_alias_default_kind
            );
            // Preserve operator-configured kinds; only fix the default.
            if model_alias_kinds.contains_key("subscriptions") {
                model_alias_default_kind = "subscriptions".to_string();
            } else if let Some(first) = model_alias_kinds.keys().next().cloned() {
                model_alias_default_kind = first;
            } else {
                model_alias_kinds = crate::aliases::AliasConfig::default_config().kinds;
                model_alias_default_kind = "subscriptions".to_string();
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
        let vercel_user_agent = first_configured([
            std::env::var("UNDERCLASS_VERCEL_USER_AGENT").ok(),
            file_cfg.vercel_user_agent,
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
            vercel_cooldown_ms,
            claude_cooldown_ms,
            cline_user_agent,
            opencode_go_user_agent,
            vercel_user_agent,
            stats_retention_days,
            stream_idle_timeout,
            upstream_connect_timeout: std::time::Duration::from_secs(upstream_connect_timeout),
            pre_output_hold: std::time::Duration::from_secs(pre_output_hold_secs),
            pre_output_hold_bytes,
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

    /// The environment must be able to turn the idle deadline off even when the file sets one, and
    /// `0` must mean "off" at either level rather than a zero-length deadline.
    #[test]
    fn an_environment_value_can_override_the_file_idle_timeout_in_either_direction() {
        let secs = |value: u64| Some(std::time::Duration::from_secs(value));
        assert_eq!(resolve_idle_timeout(None, None), None);
        assert_eq!(resolve_idle_timeout(Some(0), None), None);
        assert_eq!(resolve_idle_timeout(None, Some(0)), None);
        assert_eq!(resolve_idle_timeout(Some(0), Some(300)), None);
        assert_eq!(resolve_idle_timeout(Some(120), Some(0)), secs(120));
        assert_eq!(resolve_idle_timeout(Some(120), Some(300)), secs(120));
        assert_eq!(resolve_idle_timeout(None, Some(300)), secs(300));
    }

    /// The shipped example must show the whole configuration surface and cannot drift from the
    /// code: it parses as `FileConfig` and equals the built-in defaults exactly.
    #[test]
    fn the_example_config_parses_and_matches_the_built_in_defaults() {
        let text = include_str!("../config.example.toml");
        let parsed: FileConfig = toml::from_str(text).expect("config.example.toml must parse");
        assert_eq!(parsed, FileConfig::defaults());
    }

    /// Every field of `FileConfig` must appear as a key in the example, so adding a setting
    /// without documenting it in the example fails the build rather than leaving a silent gap.
    #[test]
    fn the_example_config_names_every_setting() {
        let text = include_str!("../config.example.toml");
        let table: toml::Table = toml::from_str(text).expect("config.example.toml must parse");
        for key in [
            "bind",
            "proxy_key",
            "ui_token",
            "codex_cooldown_secs",
            "copilot_cooldown_secs",
            "claude_cooldown_secs",
            "cline_cooldown_secs",
            "opencode_go_cooldown_secs",
            "vercel_cooldown_secs",
            "auto_codex_resets",
            "stats_retention_days",
            "stream_idle_timeout_secs",
            "upstream_connect_timeout_secs",
            "pre_output_hold_secs",
            "pre_output_hold_bytes",
            "user_agent",
            "cline_user_agent",
            "opencode_go_user_agent",
            "vercel_user_agent",
            "model_aliases",
        ] {
            assert!(table.contains_key(key), "config.example.toml omits {key}");
        }
        let aliases = table["model_aliases"].as_table().expect("a table");
        for key in ["subscriptions", "credits", "default_kind"] {
            assert!(
                aliases.contains_key(key),
                "config.example.toml omits model_aliases.{key}"
            );
        }
    }

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
