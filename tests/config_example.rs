//! Copying the shipped example config into place must change nothing: every resolved value equals
//! the compiled-in default. This runs in its own test process because it sets `UNDERCLASS_CONFIG_DIR`.

use std::io::Write;
use underclass::config::Config;

#[test]
fn copying_the_example_config_changes_no_resolved_setting() {
    let dir =
        std::env::temp_dir().join(format!("underclass-config-example-{}", std::process::id()));
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    // An empty data dir keeps the run away from the operator's real pool.db.
    let data_dir = dir.join("data");
    let mut file = std::fs::File::create(config_dir.join("config.toml")).unwrap();
    file.write_all(include_str!("../config.example.toml").as_bytes())
        .unwrap();
    drop(file);

    // SAFETY: this test binary holds a single test, so nothing else reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("UNDERCLASS_CONFIG_DIR", &config_dir);
        std::env::set_var("UNDERCLASS_DATA_DIR", &data_dir);
        for key in [
            "UNDERCLASS_BIND",
            "UNDERCLASS_PROXY_KEY",
            "UNDERCLASS_UI_TOKEN",
            "UNDERCLASS_USER_AGENT",
            "UNDERCLASS_CLINE_USER_AGENT",
            "UNDERCLASS_OPENCODE_GO_USER_AGENT",
            "UNDERCLASS_VERCEL_USER_AGENT",
            "UNDERCLASS_CLINE_COOLDOWN_SECS",
            "UNDERCLASS_OPENCODE_GO_COOLDOWN_SECS",
            "UNDERCLASS_VERCEL_COOLDOWN_SECS",
            "UNDERCLASS_AUTO_CODEX_RESETS",
            "UNDERCLASS_STATS_RETENTION_DAYS",
            "UNDERCLASS_STREAM_IDLE_TIMEOUT_SECS",
            "UNDERCLASS_UPSTREAM_CONNECT_TIMEOUT_SECS",
            "UNDERCLASS_PRE_OUTPUT_HOLD_SECS",
            "UNDERCLASS_PRE_OUTPUT_HOLD_BYTES",
        ] {
            std::env::remove_var(key);
        }
    }

    let cfg = Config::load();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(cfg.bind, "127.0.0.1:8080");
    // A blank key is "unset": the key is minted from the store on first run, not taken from the file.
    assert_eq!(cfg.proxy_key.as_deref(), Some(""));
    assert_eq!(cfg.ui_token.as_deref(), Some(""));
    assert_eq!(cfg.codex_cooldown_ms, 30 * 60 * 1000);
    assert_eq!(cfg.copilot_cooldown_ms, 30 * 60 * 1000);
    assert_eq!(cfg.claude_cooldown_ms, 30 * 60 * 1000);
    assert_eq!(cfg.cline_cooldown_ms, 30 * 60 * 1000);
    assert_eq!(cfg.opencode_go_cooldown_ms, 30 * 60 * 1000);
    assert_eq!(cfg.vercel_cooldown_ms, 30 * 60 * 1000);
    assert!(cfg.auto_codex_resets);
    assert_eq!(cfg.stats_retention_days, 730);
    // Unbounded by default: no idle deadline, and a hold that only decides when streaming starts.
    assert!(cfg.stream_idle_timeout.is_none());
    assert_eq!(
        cfg.upstream_connect_timeout,
        std::time::Duration::from_secs(10)
    );
    assert_eq!(cfg.pre_output_hold, std::time::Duration::from_secs(60));
    assert_eq!(cfg.pre_output_hold_bytes, 1024 * 1024);
    assert_eq!(cfg.cline_user_agent, None);
    assert_eq!(cfg.opencode_go_user_agent, None);
    assert_eq!(cfg.vercel_user_agent, None);
    assert_eq!(cfg.model_alias_default_kind, "subscriptions");
    assert_eq!(
        cfg.model_alias_kinds.get("subscriptions"),
        Some(&vec![
            "cline/cline-pass/".to_string(),
            "opencode-go/".to_string()
        ])
    );
    assert_eq!(
        cfg.model_alias_kinds.get("credits"),
        Some(&vec!["cline/".to_string(), "vercel/".to_string()])
    );
}
