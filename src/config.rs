use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub workspace: WorkspaceCfg,
    #[serde(default)]
    pub publish: PublishCfg,
    #[serde(default)]
    pub gate: GateCfg,
    #[serde(default)]
    pub line: LineCfg,
    #[serde(default)]
    pub orchestrator: OrchestratorCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineCfg {
    /// How far back edits are considered when hunting an error's culprit.
    #[serde(default = "default_lookback")]
    pub blame_lookback_minutes: i64,
    /// How recently another session must have written a file for a failure
    /// naming it to read as a file caught mid-edit rather than a breakage.
    /// Zero turns the check off.
    #[serde(default = "default_mid_write")]
    pub mid_write_seconds: i64,
}

fn default_lookback() -> i64 {
    120
}

fn default_mid_write() -> i64 {
    90
}

impl Default for LineCfg {
    fn default() -> Self {
        LineCfg {
            blame_lookback_minutes: default_lookback(),
            mid_write_seconds: default_mid_write(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorCfg {
    /// Off by default: deterministic rules stand alone until this is enabled.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_orc_command")]
    pub command: String,
    #[serde(default = "default_orc_model")]
    pub model: String,
    #[serde(default = "default_orc_timeout")]
    pub timeout_secs: u64,
}

fn default_orc_command() -> String {
    "claude".into()
}
fn default_orc_model() -> String {
    "haiku".into()
}
fn default_orc_timeout() -> u64 {
    20
}

impl Default for OrchestratorCfg {
    fn default() -> Self {
        OrchestratorCfg {
            enabled: false,
            command: default_orc_command(),
            model: default_orc_model(),
            timeout_secs: default_orc_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCfg {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Regions within this many lines of a target count as conflicting.
    #[serde(default = "default_margin")]
    pub margin_lines: i64,
    /// A session's regions on a file stay "hot" this long after its last edit there.
    #[serde(default = "default_presence")]
    pub presence_minutes: i64,
}

fn default_true() -> bool {
    true
}
fn default_margin() -> i64 {
    3
}
fn default_presence() -> i64 {
    30
}

impl Default for GateCfg {
    fn default() -> Self {
        GateCfg {
            enabled: true,
            margin_lines: default_margin(),
            presence_minutes: default_presence(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspaceCfg {
    /// Extra ignore patterns (gitignore syntax) appended to the shadow repo's excludes.
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishCfg {
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
    /// Optional, and usually unset: the push remote differs per clone, so it is
    /// normally read from `ortak.remote` in git config. See `publish::remote_for`.
    #[serde(default)]
    pub remote: Option<String>,
}

fn default_base_branch() -> String {
    "main".into()
}
fn default_branch_prefix() -> String {
    "task/".into()
}
impl Default for PublishCfg {
    fn default() -> Self {
        PublishCfg {
            base_branch: default_base_branch(),
            branch_prefix: default_branch_prefix(),
            remote: None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let global = global_config_path();
        Self::load_layers(global.as_deref(), path)
    }

    fn load_layers(global_path: Option<&Path>, workspace_path: &Path) -> Result<Config> {
        let mut merged = toml::Value::Table(toml::map::Map::new());
        if let Some(global_path) = global_path {
            if let Some(global) = read_config(global_path)? {
                merge_config(&mut merged, global);
            }
        }
        if let Some(workspace) = read_config(workspace_path)? {
            merge_config(&mut merged, workspace);
        }
        merged
            .try_into()
            .context("could not combine global and workspace configuration")
    }

    /// The file `ortak init` writes. The base branch is passed in rather than
    /// spelled here, because a repository that calls its trunk something other
    /// than `main` used to get a config naming a branch it does not have, and
    /// the first publish its owner ever ran failed on it.
    pub fn default_toml(base_branch: &str) -> String {
        format!(
            r#"# ortak configuration

[workspace]
# Extra patterns to exclude from the journal (gitignore syntax).
# ortak also applies the project's .gitignore file.
ignore = []

[publish]
# Base branch for published branches
base_branch = "{base_branch}"
# Prefix for generated branch names
branch_prefix = "task/"
# Remote used for pushes. Leave it unset: the remote differs per clone, so set
# yours with `git config ortak.remote <name>`. Defaults to origin.
# remote = "origin"

[gate]
# Reject edits inside another session's active region
enabled = true
# Treat lines within this margin as conflicting
margin_lines = 3
# Keep regions hot for this many minutes after the owner's last edit to the file
presence_minutes = 30

[line]
# Search this many minutes of edit history when assigning an error
blame_lookback_minutes = 120
# Decline to stop the line when the failure names a file another session wrote
# this recently: a half-written file fails and then builds again on its own.
# Set to 0 to report regardless.
mid_write_seconds = 90

# Optional workspace overrides for the LLM arbiter. Global defaults can live in
# ~/.ortak/config.toml. Uncomment this block to override them in this workspace.
# [orchestrator]
# enabled = true
# command = "claude"
# model = "haiku"
# timeout_secs = 20
"#
        )
    }
}

fn global_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".ortak").join("config.toml"))
}

fn read_config(path: &Path) -> Result<Option<toml::Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&raw).with_context(|| format!("could not parse {}", path.display()))?;
    let _: Config = value
        .clone()
        .try_into()
        .with_context(|| format!("could not parse {}", path.display()))?;
    Ok(Some(value))
}

fn merge_config(base: &mut toml::Value, override_value: toml::Value) {
    match (base, override_value) {
        (toml::Value::Table(base), toml::Value::Table(overrides)) => {
            for (key, value) in overrides {
                match base.get_mut(&key) {
                    Some(existing) => merge_config(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, override_value) => *base = override_value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ortak-config-{name}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn workspace_fields_override_global_fields() {
        let dir = temp_dir("precedence");
        let global = dir.join("global.toml");
        let workspace = dir.join("workspace.toml");
        std::fs::write(
            &global,
            r#"[orchestrator]
enabled = true
command = "claude"
model = "sonnet"
timeout_secs = 45

[gate]
margin_lines = 8
"#,
        )
        .unwrap();
        std::fs::write(
            &workspace,
            r#"[orchestrator]
model = "haiku"

[gate]
presence_minutes = 12
"#,
        )
        .unwrap();

        let cfg = Config::load_layers(Some(&global), &workspace).unwrap();

        assert!(cfg.orchestrator.enabled);
        assert_eq!(cfg.orchestrator.command, "claude");
        assert_eq!(cfg.orchestrator.model, "haiku");
        assert_eq!(cfg.orchestrator.timeout_secs, 45);
        assert_eq!(cfg.gate.margin_lines, 8);
        assert_eq!(cfg.gate.presence_minutes, 12);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_can_explicitly_disable_a_global_orchestrator() {
        let dir = temp_dir("disable");
        let global = dir.join("global.toml");
        let workspace = dir.join("workspace.toml");
        std::fs::write(&global, "[orchestrator]\nenabled = true\n").unwrap();
        std::fs::write(&workspace, "[orchestrator]\nenabled = false\n").unwrap();

        let cfg = Config::load_layers(Some(&global), &workspace).unwrap();

        assert!(!cfg.orchestrator.enabled);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_config_works_without_a_workspace_file() {
        let dir = temp_dir("global-only");
        let global = dir.join("global.toml");
        let workspace = dir.join("missing.toml");
        std::fs::write(
            &global,
            "[orchestrator]\nenabled = true\nmodel = \"sonnet\"\n",
        )
        .unwrap();

        let cfg = Config::load_layers(Some(&global), &workspace).unwrap();

        assert!(cfg.orchestrator.enabled);
        assert_eq!(cfg.orchestrator.model, "sonnet");
        assert!(cfg.gate.enabled);
        assert_eq!(cfg.gate.margin_lines, 3);
        assert_eq!(cfg.gate.presence_minutes, 30);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_global_values_are_not_hidden_by_workspace_overrides() {
        let dir = temp_dir("invalid-global");
        let global = dir.join("global.toml");
        let workspace = dir.join("workspace.toml");
        std::fs::write(&global, "[orchestrator]\ntimeout_secs = \"slow\"\n").unwrap();
        std::fs::write(&workspace, "[orchestrator]\ntimeout_secs = 20\n").unwrap();

        let error = Config::load_layers(Some(&global), &workspace)
            .unwrap_err()
            .to_string();

        assert!(error.contains(&global.display().to_string()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn generated_workspace_config_inherits_global_orchestrator_settings() {
        let generated = Config::default_toml("main");
        let value: toml::Value = toml::from_str(&generated).unwrap();

        assert!(value.get("orchestrator").is_none());
        assert!(generated.contains("# ~/.ortak/config.toml"));
    }
}
