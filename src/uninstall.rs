use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

const PLUGIN_ID: &str = "ortak@ortak";
const MARKETPLACE: &str = "ortak";

pub fn run() -> Result<()> {
    let executable =
        std::env::current_exe().context("could not locate the current ortak binary")?;
    run_with(&mut System, &executable)
}

fn run_with(runtime: &mut impl Runtime, executable: &Path) -> Result<()> {
    validate_binary_path(executable)?;

    let mut failures = Vec::new();
    attempt("Codex", &mut failures, || uninstall_codex(runtime));
    attempt("Claude Code", &mut failures, || uninstall_claude(runtime));
    attempt("OpenCode", &mut failures, || runtime.uninstall_opencode());

    if !failures.is_empty() {
        anyhow::bail!(
            "uninstall incomplete; the ortak binary was left at {} so you can retry:\n  - {}",
            executable.display(),
            failures.join("\n  - ")
        );
    }

    runtime.remove_binary(executable).with_context(|| {
        format!(
            "could not remove the ortak binary at {}",
            executable.display()
        )
    })?;
    println!("Removed ortak binary: {}", executable.display());
    println!(
        "\nOrtak is uninstalled. Restart active agent sessions to unload its hooks and skills."
    );
    println!("Existing ortak.toml and .ortak workspace data were left in place.");
    Ok(())
}

fn attempt(label: &str, failures: &mut Vec<String>, action: impl FnOnce() -> Result<()>) {
    if let Err(error) = action() {
        failures.push(format!("{label}: {error:#}"));
    }
}

fn uninstall_codex(runtime: &mut impl Runtime) -> Result<()> {
    if !runtime.available("codex") {
        println!("Codex not found; skipping its Ortak plugin and marketplace.");
        return Ok(());
    }

    let plugins = json_output(runtime, "codex", &["plugin", "list", "--json"])?;
    if plugin_scopes(&plugins).is_empty() {
        println!("Ortak is not installed in Codex; skipping its plugin.");
    } else {
        println!("Removing the Codex plugin and bundled skill...");
        checked(runtime, "codex", &["plugin", "remove", PLUGIN_ID])?;
        println!("Codex plugin and bundled skill removed.");
    }

    let marketplaces = json_output(
        runtime,
        "codex",
        &["plugin", "marketplace", "list", "--json"],
    )?;
    if has_marketplace(&marketplaces) {
        checked(
            runtime,
            "codex",
            &["plugin", "marketplace", "remove", MARKETPLACE],
        )?;
        println!("Codex Ortak marketplace removed.");
    } else {
        println!("Ortak marketplace is not configured in Codex; skipping it.");
    }
    Ok(())
}

fn uninstall_claude(runtime: &mut impl Runtime) -> Result<()> {
    if !runtime.available("claude") {
        println!("Claude Code not found; skipping its Ortak plugin and marketplace.");
        return Ok(());
    }

    let plugins = json_output(runtime, "claude", &["plugin", "list", "--json"])?;
    let scopes = plugin_scopes(&plugins);
    if scopes.is_empty() {
        println!("Ortak is not installed in Claude Code; skipping its plugin.");
    } else {
        println!("Removing the Claude Code plugin and bundled skill...");
        for scope in scopes {
            match scope {
                Some(scope) => checked(
                    runtime,
                    "claude",
                    &["plugin", "uninstall", "--yes", "--scope", &scope, PLUGIN_ID],
                )?,
                None => checked(
                    runtime,
                    "claude",
                    &["plugin", "uninstall", "--yes", PLUGIN_ID],
                )?,
            }
        }
        println!("Claude Code plugin and bundled skill removed.");
    }

    let marketplaces = json_output(
        runtime,
        "claude",
        &["plugin", "marketplace", "list", "--json"],
    )?;
    if has_marketplace(&marketplaces) {
        // Omitting --scope is intentional: Claude Code removes this named
        // marketplace declaration from every scope, matching the plugin scopes
        // handled above.
        checked(
            runtime,
            "claude",
            &["plugin", "marketplace", "remove", MARKETPLACE],
        )?;
        println!("Claude Code Ortak marketplace removed.");
    } else {
        println!("Ortak marketplace is not configured in Claude Code; skipping it.");
    }
    Ok(())
}

fn json_output(runtime: &mut impl Runtime, program: &str, args: &[&str]) -> Result<Value> {
    let output = runtime.output(OsStr::new(program), args)?;
    if !output.success {
        anyhow::bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            output.failure()
        );
    }
    serde_json::from_str(&output.stdout).with_context(|| {
        format!(
            "{program} returned invalid JSON for `{program} {}`",
            args.join(" ")
        )
    })
}

fn plugin_scopes(value: &Value) -> BTreeSet<Option<String>> {
    let mut scopes = BTreeSet::new();
    collect_plugin_scopes(value, &mut scopes);
    scopes
}

fn collect_plugin_scopes(value: &Value, scopes: &mut BTreeSet<Option<String>>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_plugin_scopes(item, scopes);
            }
        }
        Value::Object(object) => {
            let is_ortak = ["pluginId", "id"]
                .iter()
                .filter_map(|key| object.get(*key).and_then(Value::as_str))
                .any(|id| id == PLUGIN_ID)
                && object.get("installed").and_then(Value::as_bool) != Some(false);
            if is_ortak {
                scopes.insert(
                    object
                        .get("scope")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                );
            } else {
                for child in object.values() {
                    collect_plugin_scopes(child, scopes);
                }
            }
        }
        _ => {}
    }
}

fn has_marketplace(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(has_marketplace),
        Value::Object(object) => {
            object.get("name").and_then(Value::as_str) == Some(MARKETPLACE)
                || object.values().any(has_marketplace)
        }
        _ => false,
    }
}

fn checked(runtime: &mut impl Runtime, program: &str, args: &[&str]) -> Result<()> {
    let output = runtime.output(OsStr::new(program), args)?;
    if !output.stdout.is_empty() {
        print!("{}", output.stdout);
    }
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    if !output.success {
        anyhow::bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            output.failure()
        );
    }
    Ok(())
}

fn validate_binary_path(executable: &Path) -> Result<()> {
    if executable.file_name() != Some(OsStr::new("ortak")) {
        anyhow::bail!(
            "the running executable is named {:?}, not \"ortak\"; refusing to remove it",
            executable.file_name().unwrap_or_default()
        );
    }
    Ok(())
}

struct CommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

impl CommandOutput {
    fn failure(&self) -> &str {
        let stderr = self.stderr.trim();
        if stderr.is_empty() {
            "command exited unsuccessfully without an error message"
        } else {
            stderr
        }
    }
}

trait Runtime {
    fn available(&self, program: &str) -> bool;
    fn output(&mut self, program: &OsStr, args: &[&str]) -> Result<CommandOutput>;
    fn uninstall_opencode(&mut self) -> Result<()>;
    fn remove_binary(&mut self, path: &Path) -> Result<()>;
}

struct System;

impl Runtime for System {
    fn available(&self, program: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| executable_file(&dir.join(program)))
    }

    fn output(&mut self, program: &OsStr, args: &[&str]) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("could not run {}", Path::new(program).display()))?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn uninstall_opencode(&mut self) -> Result<()> {
        crate::opencode::uninstall()
    }

    fn remove_binary(&mut self, path: &Path) -> Result<()> {
        std::fs::remove_file(path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, VecDeque};
    use std::path::PathBuf;

    #[test]
    fn removes_both_plugins_their_marketplaces_and_the_binary() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.responses = VecDeque::from([
            codex_plugin(),
            ok("codex plugin removed\n"),
            marketplace(),
            ok("codex marketplace removed\n"),
            claude_plugins(),
            ok("claude user plugin removed\n"),
            ok("claude project plugin removed\n"),
            marketplace(),
            ok("claude marketplace removed\n"),
        ]);

        run_with(&mut runtime, Path::new("/opt/ortak/bin/ortak")).unwrap();

        assert_eq!(
            runtime.calls,
            [
                "codex plugin list --json",
                "codex plugin remove ortak@ortak",
                "codex plugin marketplace list --json",
                "codex plugin marketplace remove ortak",
                "claude plugin list --json",
                "claude plugin uninstall --yes --scope project ortak@ortak",
                "claude plugin uninstall --yes --scope user ortak@ortak",
                "claude plugin marketplace list --json",
                "claude plugin marketplace remove ortak",
            ]
        );
        assert_eq!(
            runtime.removed_binary,
            Some(PathBuf::from("/opt/ortak/bin/ortak"))
        );
        assert!(runtime.opencode_uninstalled);
    }

    #[test]
    fn is_idempotent_when_agent_components_are_absent() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.responses = VecDeque::from([
            ok(r#"{"installed":[]}"#),
            ok(r#"{"marketplaces":[]}"#),
            ok("[]"),
            ok("[]"),
        ]);

        run_with(&mut runtime, Path::new("/tmp/ortak")).unwrap();

        assert_eq!(runtime.calls.len(), 4);
        assert_eq!(runtime.removed_binary, Some(PathBuf::from("/tmp/ortak")));
    }

    #[test]
    fn missing_agent_clis_do_not_block_binary_removal() {
        let mut runtime = Fake::with_programs(&[]);

        run_with(&mut runtime, Path::new("/tmp/ortak")).unwrap();

        assert!(runtime.calls.is_empty());
        assert!(runtime.opencode_uninstalled);
        assert_eq!(runtime.removed_binary, Some(PathBuf::from("/tmp/ortak")));
    }

    #[test]
    fn a_plugin_failure_does_not_block_the_other_agent_but_keeps_the_binary() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.responses = VecDeque::from([
            codex_plugin(),
            failed("codex removal failed"),
            claude_plugins(),
            ok("claude project plugin removed\n"),
            ok("claude user plugin removed\n"),
            marketplace(),
            ok("claude marketplace removed\n"),
        ]);

        let error = run_with(&mut runtime, Path::new("/tmp/ortak"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("Codex: `codex plugin remove ortak@ortak` failed"));
        assert!(runtime
            .calls
            .contains(&"claude plugin marketplace remove ortak".to_string()));
        assert!(runtime.opencode_uninstalled);
        assert_eq!(runtime.removed_binary, None);
    }

    #[test]
    fn rejects_a_renamed_executable_before_changing_anything() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);

        let error = run_with(&mut runtime, Path::new("/tmp/ortak-debug"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("refusing to remove it"));
        assert!(runtime.calls.is_empty());
        assert!(!runtime.opencode_uninstalled);
        assert_eq!(runtime.removed_binary, None);
    }

    fn codex_plugin() -> CommandOutput {
        ok(r#"{"installed":[{"pluginId":"ortak@ortak","installed":true}]}"#)
    }

    fn claude_plugins() -> CommandOutput {
        ok(r#"[{"id":"ortak@ortak","scope":"user"},{"id":"ortak@ortak","scope":"project"}]"#)
    }

    fn marketplace() -> CommandOutput {
        ok(r#"{"marketplaces":[{"name":"ortak"}]}"#)
    }

    fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn failed(stderr: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    struct Fake {
        programs: HashSet<String>,
        responses: VecDeque<CommandOutput>,
        calls: Vec<String>,
        opencode_uninstalled: bool,
        removed_binary: Option<PathBuf>,
    }

    impl Fake {
        fn with_programs(programs: &[&str]) -> Self {
            Self {
                programs: programs
                    .iter()
                    .map(|program| (*program).to_string())
                    .collect(),
                responses: VecDeque::new(),
                calls: Vec::new(),
                opencode_uninstalled: false,
                removed_binary: None,
            }
        }
    }

    impl Runtime for Fake {
        fn available(&self, program: &str) -> bool {
            self.programs.contains(program)
        }

        fn output(&mut self, program: &OsStr, args: &[&str]) -> Result<CommandOutput> {
            self.calls
                .push(format!("{} {}", program.to_string_lossy(), args.join(" ")));
            self.responses
                .pop_front()
                .context("test did not provide a command response")
        }

        fn uninstall_opencode(&mut self) -> Result<()> {
            self.opencode_uninstalled = true;
            Ok(())
        }

        fn remove_binary(&mut self, path: &Path) -> Result<()> {
            self.removed_binary = Some(path.to_path_buf());
            Ok(())
        }
    }
}
