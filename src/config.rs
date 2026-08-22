use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
}

fn default_lookback() -> i64 {
    120
}

impl Default for LineCfg {
    fn default() -> Self {
        LineCfg {
            blame_lookback_minutes: default_lookback(),
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
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("could not parse {}", path.display()))
    }

    pub fn default_toml() -> &'static str {
        r#"# ortak configuration

[workspace]
# Extra patterns to exclude from the journal (gitignore syntax).
# ortak also applies the project's .gitignore file.
ignore = []

[publish]
# Base branch for published branches
base_branch = "main"
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

[orchestrator]
# LLM arbiter for conflicts and ambiguous error ownership. Disabled by default.
# Deterministic rules apply while disabled.
enabled = false
command = "claude"
model = "haiku"
timeout_secs = 20
"#
    }
}
