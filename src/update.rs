use anyhow::{Context, Result};
use serde_json::Value;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const INSTALLER: &str = include_str!("../install.sh");
const PLUGIN_ID: &str = "ortak@ortak";
const MARKETPLACE: &str = "ortak";

pub fn run() -> Result<()> {
    let executable =
        std::env::current_exe().context("could not locate the current ortak binary")?;
    run_with(&mut System, &executable)
}

fn run_with(runtime: &mut impl Runtime, executable: &Path) -> Result<()> {
    let mut failures = Vec::new();

    attempt("binary", &mut failures, || {
        update_binary(runtime, executable)
    });
    attempt("Codex plugin", &mut failures, || update_codex(runtime));
    attempt("Claude plugin", &mut failures, || update_claude(runtime));

    if !failures.is_empty() {
        anyhow::bail!("update incomplete:\n  - {}", failures.join("\n  - "));
    }

    println!(
        "\nortak is up to date. Restart active agent sessions to load updated hooks and skills."
    );
    Ok(())
}

fn attempt(label: &str, failures: &mut Vec<String>, update: impl FnOnce() -> Result<()>) {
    if let Err(error) = update() {
        failures.push(format!("{label}: {error:#}"));
    }
}

fn update_binary(runtime: &mut impl Runtime, executable: &Path) -> Result<()> {
    let install_dir = install_dir_for(executable)?;
    println!("Updating ortak binary at {}...", executable.display());
    runtime.install_binary(install_dir)?;

    let output = runtime.output(executable.as_os_str(), &["--version"])?;
    if !output.success {
        anyhow::bail!(
            "the installed binary could not be verified: {}",
            output.failure()
        );
    }
    let version = output.stdout.trim();
    if !version.starts_with("ortak ") {
        anyhow::bail!("the installed binary returned an unexpected version: {version:?}");
    }
    println!("Binary updated: {version}");
    Ok(())
}

fn install_dir_for(executable: &Path) -> Result<&Path> {
    if executable.file_name() != Some(OsStr::new("ortak")) {
        anyhow::bail!(
            "the running executable is named {:?}, not \"ortak\"; install an official release before using self-update",
            executable.file_name().unwrap_or_default()
        );
    }
    executable
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("the current ortak binary has no installation directory")
}

fn update_codex(runtime: &mut impl Runtime) -> Result<()> {
    if !runtime.available("codex") {
        println!("Codex not found; skipping its Ortak plugin.");
        return Ok(());
    }
    if !installed(runtime, "codex")? {
        println!("Ortak is not installed in Codex; skipping it.");
        return Ok(());
    }

    println!("Updating the Codex plugin and bundled skill...");
    // Codex has no separate plugin-update command. Refreshing a configured
    // marketplace also refreshes the installed plugin cache sourced from it.
    checked(
        runtime,
        "codex",
        &["plugin", "marketplace", "upgrade", MARKETPLACE],
    )?;
    println!("Codex plugin and skill updated.");
    Ok(())
}

fn update_claude(runtime: &mut impl Runtime) -> Result<()> {
    if !runtime.available("claude") {
        println!("Claude Code not found; skipping its Ortak plugin.");
        return Ok(());
    }
    if !installed(runtime, "claude")? {
        println!("Ortak is not installed in Claude Code; skipping it.");
        return Ok(());
    }

    println!("Updating the Claude Code plugin and bundled skill...");
    checked(
        runtime,
        "claude",
        &["plugin", "marketplace", "update", MARKETPLACE],
    )?;
    checked(runtime, "claude", &["plugin", "update", "--yes", PLUGIN_ID])?;
    println!("Claude Code plugin and skill updated.");
    Ok(())
}

fn installed(runtime: &mut impl Runtime, program: &str) -> Result<bool> {
    let output = runtime.output(OsStr::new(program), &["plugin", "list", "--json"])?;
    if !output.success {
        anyhow::bail!("could not list installed plugins: {}", output.failure());
    }
    let value: Value = serde_json::from_str(&output.stdout)
        .with_context(|| format!("{program} returned invalid plugin JSON"))?;
    Ok(contains_plugin(&value))
}

fn contains_plugin(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_plugin),
        Value::Object(object) => {
            let is_ortak = ["pluginId", "id"]
                .iter()
                .filter_map(|key| object.get(*key).and_then(Value::as_str))
                .any(|id| id == PLUGIN_ID)
                && object.get("installed").and_then(Value::as_bool) != Some(false);
            is_ortak || object.values().any(contains_plugin)
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
    fn install_binary(&mut self, install_dir: &Path) -> Result<()>;
    fn output(&mut self, program: &OsStr, args: &[&str]) -> Result<CommandOutput>;
}

struct System;

impl Runtime for System {
    fn available(&self, program: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| executable_file(&dir.join(program)))
    }

    fn install_binary(&mut self, install_dir: &Path) -> Result<()> {
        let mut child = Command::new("sh")
            .arg("-s")
            .env("ORTAK_INSTALL_DIR", install_dir)
            .env_remove("ORTAK_VERSION")
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("could not start the embedded ortak installer")?;
        let write_result = child
            .stdin
            .take()
            .context("could not open the installer input")?
            .write_all(INSTALLER.as_bytes())
            .context("could not send the embedded installer to sh");
        let status = child.wait().context("could not wait for the installer")?;
        write_result?;
        if !status.success() {
            anyhow::bail!("the embedded installer exited with {status}");
        }
        Ok(())
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
    fn recognizes_both_plugin_list_shapes() {
        let codex = serde_json::json!({
            "installed": [{"pluginId": "ortak@ortak"}]
        });
        let claude = serde_json::json!([
            {"id": "something@else"},
            {"id": "ortak@ortak"}
        ]);
        assert!(contains_plugin(&codex));
        assert!(contains_plugin(&claude));
        assert!(!contains_plugin(&serde_json::json!({"installed": []})));
        assert!(!contains_plugin(&serde_json::json!({
            "available": [{"pluginId": "ortak@ortak", "installed": false}]
        })));
    }

    #[test]
    fn updates_the_current_binary_and_both_installed_plugins() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.responses = VecDeque::from([
            ok("ortak 0.2.0\n"),
            ok(r#"{"installed":[{"pluginId":"ortak@ortak"}]}"#),
            ok("codex updated\n"),
            ok(r#"[{"id":"ortak@ortak"}]"#),
            ok("marketplace updated\n"),
            ok("plugin updated\n"),
        ]);

        run_with(&mut runtime, Path::new("/opt/ortak/bin/ortak")).unwrap();

        assert_eq!(runtime.installed_in, Some(PathBuf::from("/opt/ortak/bin")));
        assert_eq!(
            runtime.calls,
            [
                "/opt/ortak/bin/ortak --version",
                "codex plugin list --json",
                "codex plugin marketplace upgrade ortak",
                "claude plugin list --json",
                "claude plugin marketplace update ortak",
                "claude plugin update --yes ortak@ortak",
            ]
        );
    }

    #[test]
    fn one_failure_does_not_prevent_the_other_updates() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.install_error = true;
        runtime.responses = VecDeque::from([
            ok(r#"{"installed":[{"pluginId":"ortak@ortak"}]}"#),
            failed("codex failed"),
            ok(r#"[{"id":"ortak@ortak"}]"#),
            ok("marketplace updated\n"),
            ok("plugin updated\n"),
        ]);

        let error = run_with(&mut runtime, Path::new("/opt/ortak/bin/ortak"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("binary: simulated installer failure"));
        assert!(error.contains("Codex plugin:"));
        assert!(runtime
            .calls
            .contains(&"claude plugin update --yes ortak@ortak".to_string()));
    }

    #[test]
    fn rejects_a_renamed_executable() {
        let error = install_dir_for(Path::new("/tmp/ortak-debug"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not \"ortak\""));
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
        installed_in: Option<PathBuf>,
        install_error: bool,
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
                installed_in: None,
                install_error: false,
            }
        }
    }

    impl Runtime for Fake {
        fn available(&self, program: &str) -> bool {
            self.programs.contains(program)
        }

        fn install_binary(&mut self, install_dir: &Path) -> Result<()> {
            self.installed_in = Some(install_dir.to_path_buf());
            if self.install_error {
                anyhow::bail!("simulated installer failure");
            }
            Ok(())
        }

        fn output(&mut self, program: &OsStr, args: &[&str]) -> Result<CommandOutput> {
            self.calls
                .push(format!("{} {}", program.to_string_lossy(), args.join(" ")));
            self.responses
                .pop_front()
                .context("test did not provide a command response")
        }
    }
}
