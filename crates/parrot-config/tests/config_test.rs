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
denylist = ["rm -rf /"]
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
denylist = []
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