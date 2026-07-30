use parrot_config::AppConfig;

#[test]
fn default_config_loads() {
    // No parrot.toml present -> use defaults
    let config = AppConfig::load().expect("should load defaults");
    assert_eq!(config.daemon.port, 9876);
    assert!(!config.tools.shell_allowed);
    assert!(!config.tools.file_write_allowed);
    assert!(config.tools.web_allowed);
}

#[test]
fn parse_toml_config() {
    let toml_str = r#"
[daemon]
host = "0.0.0.0"
port = 9999
auth_token_file = "/tmp/test-token"

[[providers]]
id = "anthropic"
api_key = "sk-test-key"
default_model = "claude-sonnet-4-6"

[tools]
shell_allowed = false
file_write_allowed = true
web_allowed = true
max_file_size_mb = 20

[tools.sandbox]
working_dir = "/workspace"
allowlist = ["ls", "cat"]
require_confirmation = ["git push"]

[session]
data_dir = "/tmp/parrot-data"
max_history_tokens = 50000
keep_recent_turns = 4
"#;
    let config: AppConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.daemon.port, 9999);
    assert_eq!(config.providers.len(), 1);
    assert_eq!(config.providers[0].id, "anthropic");
    assert!(config.tools.file_write_allowed);
    assert_eq!(config.tools.max_file_size_mb, 20);
    assert_eq!(config.session.keep_recent_turns, 4);
}

#[test]
fn env_var_resolution() {
    std::env::set_var("TEST_PARROT_KEY", "resolved-key-123");
    let toml_str = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "/tmp/token"

[[providers]]
id = "anthropic"
api_key = "${TEST_PARROT_KEY}"
default_model = "claude-sonnet-4-6"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = "/tmp/parrot"
max_history_tokens = 100000
keep_recent_turns = 6
"#;
    let mut config: AppConfig = toml::from_str(toml_str).unwrap();
    config.resolve_env_vars().unwrap();
    assert_eq!(config.providers[0].api_key, "resolved-key-123");
    std::env::remove_var("TEST_PARROT_KEY");
}

#[test]
fn hooks_external_parses_full_subtable() {
    let toml_str = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "/tmp/parrot/token"

[[providers]]
id = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
default_model = "claude-x"

[tools]
shell_allowed = true
file_write_allowed = true
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = "/tmp/parrot"
max_history_tokens = 100000
keep_recent_turns = 6

[[hooks.external]]
id = "compliance-log"
command = ["python", "/home/me/hook.py"]
events = ["tool_call", "tool_result"]
timeout_seconds = 2

[hooks.external.config]
sink = "stderr"
"#;
    let cfg: AppConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.hooks.external.len(), 1);
    let ext = &cfg.hooks.external[0];
    assert_eq!(ext.id, "compliance-log");
    assert_eq!(
        ext.command,
        vec!["python".to_string(), "/home/me/hook.py".to_string()]
    );
    assert_eq!(
        ext.events,
        vec!["tool_call".to_string(), "tool_result".to_string()]
    );
    assert_eq!(ext.timeout_seconds, Some(2));
    assert!(ext
        .config
        .as_table()
        .map(|t| t.contains_key("sink"))
        .unwrap_or(false));
}

#[test]
fn hooks_external_multi_entries_and_optional_fields() {
    let toml_str = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "/tmp/parrot/token"

[[providers]]
id = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
default_model = "claude-x"

[tools]
shell_allowed = true
file_write_allowed = true
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = "/tmp/parrot"
max_history_tokens = 100000
keep_recent_turns = 6

[[hooks.external]]
id = "audit"
command = ["node", "/etc/parrot/audit.js"]
events = ["agent_start", "agent_end"]

[[hooks.external]]
id = "blocker"
command = ["./blocker.sh"]
events = ["tool_call"]
"#;
    let cfg: AppConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.hooks.external.len(), 2);
    assert_eq!(cfg.hooks.external[0].id, "audit");
    assert_eq!(cfg.hooks.external[0].timeout_seconds, None);
    assert_eq!(
        cfg.hooks.external[0].config,
        toml::Value::Table(toml::value::Table::new())
    );
    assert_eq!(cfg.hooks.external[1].id, "blocker");
}

#[test]
fn hooks_external_absent_backwards_compat() {
    // A config without any [[hooks.external]] must remain loadable; the
    // external field defaults to an empty Vec (Global Constraints §8).
    let toml_str = r#"
[daemon]
host = "127.0.0.1"
port = 9876
auth_token_file = "/tmp/parrot/token"

[[providers]]
id = "anthropic"
api_key = "key"
default_model = "claude-x"

[tools]
shell_allowed = false
file_write_allowed = false
web_allowed = true
max_file_size_mb = 10

[tools.sandbox]
working_dir = "."
allowlist = []
require_confirmation = []

[session]
data_dir = "/tmp/parrot"
max_history_tokens = 100000
keep_recent_turns = 6
"#;
    let cfg: AppConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.hooks.external.is_empty());
}
