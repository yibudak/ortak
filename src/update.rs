use anyhow::{Context, Result};
use serde_json::Value;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const INSTALLER: &str = include_str!("../install.sh");
const PLUGIN_ID: &str = "ortak@ortak";
const MARKETPLACE: &str = "ortak";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/yibudak/ortak/releases/latest";

pub fn run() -> Result<()> {
    let executable =
        std::env::current_exe().context("could not locate the current ortak binary")?;
    run_with(&mut System, &executable)
}

fn run_with(runtime: &mut impl Runtime, executable: &Path) -> Result<()> {
    let latest = latest_version(runtime)?;
    let mut failures = Vec::new();
    let mut updated = false;

    attempt("binary", &mut failures, &mut updated, || {
        update_binary(runtime, executable, &latest)
    });
    attempt("Codex plugin", &mut failures, &mut updated, || {
        update_codex(runtime, &latest)
    });
    attempt("Claude plugin", &mut failures, &mut updated, || {
        update_claude(runtime, &latest)
    });
    attempt("OpenCode plugin", &mut failures, &mut updated, || {
        update_opencode(runtime, executable, &latest)
    });

    if !failures.is_empty() {
        anyhow::bail!("update incomplete:\n  - {}", failures.join("\n  - "));
    }

    if updated {
        println!(
            "\nortak is up to date. Restart active agent sessions to load updated hooks and skills."
        );
    } else {
        println!("\nortak is already up to date.");
    }
    Ok(())
}

fn latest_version(runtime: &mut impl Runtime) -> Result<String> {
    let output = runtime.output(
        OsStr::new("curl"),
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            LATEST_RELEASE_URL,
        ],
    )?;
    if !output.success {
        anyhow::bail!("could not check the latest release: {}", output.failure());
    }
    let value: Value =
        serde_json::from_str(&output.stdout).context("GitHub returned invalid release JSON")?;
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .context("GitHub's latest release has no tag_name")?;
    let version = normalize_version(tag).context("GitHub returned an invalid release tag")?;
    println!("Latest ortak release: {version}");
    Ok(version.to_string())
}

fn normalize_version(version: &str) -> Option<&str> {
    let version = version.trim().strip_prefix('v').unwrap_or(version.trim());
    (!version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+')))
    .then_some(version)
}

fn attempt(
    label: &str,
    failures: &mut Vec<String>,
    updated: &mut bool,
    update: impl FnOnce() -> Result<bool>,
) {
    match update() {
        Ok(changed) => *updated |= changed,
        Err(error) => failures.push(format!("{label}: {error:#}")),
    }
}

fn update_binary(runtime: &mut impl Runtime, executable: &Path, latest: &str) -> Result<bool> {
    if CURRENT_VERSION == latest {
        println!("Binary already up to date: ortak {CURRENT_VERSION}");
        return Ok(false);
    }
    let install_dir = install_dir_for(executable)?;
    println!(
        "Updating ortak binary from {CURRENT_VERSION} to {latest} at {}...",
        executable.display()
    );
    runtime.install_binary(install_dir)?;

    let output = runtime.output(executable.as_os_str(), &["--version"])?;
    if !output.success {
        anyhow::bail!(
            "the installed binary could not be verified: {}",
            output.failure()
        );
    }
    let version = output.stdout.trim();
    let expected = format!("ortak {latest}");
    if version != expected {
        anyhow::bail!("the installed binary returned {version:?}; expected {expected:?}");
    }
    println!("Binary updated: {version}");
    Ok(true)
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

fn update_codex(runtime: &mut impl Runtime, latest: &str) -> Result<bool> {
    if !runtime.available("codex") {
        println!("Codex not found; skipping its Ortak plugin.");
        return Ok(false);
    }
    let Some(installed) = installed_plugin(runtime, "codex")? else {
        println!("Ortak is not installed in Codex; skipping it.");
        return Ok(false);
    };
    if installed.matches(latest) {
        println!("Codex plugin and skill already up to date: {latest}");
        return Ok(false);
    }

    println!(
        "Updating the Codex plugin and bundled skill from {} to {latest}...",
        installed.display()
    );
    // Codex has no separate plugin-update command. Refreshing a configured
    // marketplace also refreshes the installed plugin cache sourced from it.
    checked(
        runtime,
        "codex",
        &["plugin", "marketplace", "upgrade", MARKETPLACE],
    )?;
    verify_plugin(runtime, "codex", latest)?;
    println!("Codex plugin and skill updated.");
    Ok(true)
}

fn update_claude(runtime: &mut impl Runtime, latest: &str) -> Result<bool> {
    if !runtime.available("claude") {
        println!("Claude Code not found; skipping its Ortak plugin.");
        return Ok(false);
    }
    let Some(installed) = installed_plugin(runtime, "claude")? else {
        println!("Ortak is not installed in Claude Code; skipping it.");
        return Ok(false);
    };
    if installed.matches(latest) {
        println!("Claude Code plugin and skill already up to date: {latest}");
        return Ok(false);
    }

    println!(
        "Updating the Claude Code plugin and bundled skill from {} to {latest}...",
        installed.display()
    );
    checked(
        runtime,
        "claude",
        &["plugin", "marketplace", "update", MARKETPLACE],
    )?;
    checked(runtime, "claude", &["plugin", "update", "--yes", PLUGIN_ID])?;
    verify_plugin(runtime, "claude", latest)?;
    println!("Claude Code plugin and skill updated.");
    Ok(true)
}

fn update_opencode(runtime: &mut impl Runtime, executable: &Path, latest: &str) -> Result<bool> {
    if !runtime.available("opencode") {
        println!("OpenCode not found; skipping its Ortak plugin.");
        return Ok(false);
    }
    let Some(installed) = runtime.opencode_plugin_version()? else {
        println!("Ortak is not installed in OpenCode; skipping it.");
        return Ok(false);
    };
    if normalize_version(&installed) == Some(latest) {
        println!("OpenCode plugin and skill already up to date: {latest}");
        return Ok(false);
    }

    println!("Updating the OpenCode plugin and skill from {installed} to {latest}...");
    // The binary may have replaced itself earlier in this run. Invoke the path
    // again so the newly installed binary writes its matching embedded adapter.
    runtime.install_opencode_plugin(executable)?;
    let version = runtime
        .opencode_plugin_version()?
        .context("OpenCode's Ortak plugin disappeared after the update")?;
    if normalize_version(&version) != Some(latest) {
        anyhow::bail!("OpenCode reports plugin version {version}; expected {latest}");
    }
    println!("OpenCode plugin and skill updated.");
    Ok(true)
}

fn verify_plugin(runtime: &mut impl Runtime, program: &str, expected: &str) -> Result<()> {
    let installed = installed_plugin(runtime, program)?
        .with_context(|| format!("{PLUGIN_ID} disappeared after the update"))?;
    if !installed.matches(expected) {
        anyhow::bail!(
            "{program} reports plugin version {}; expected {expected}",
            installed.display()
        );
    }
    Ok(())
}

fn installed_plugin(runtime: &mut impl Runtime, program: &str) -> Result<Option<InstalledPlugin>> {
    let output = runtime.output(OsStr::new(program), &["plugin", "list", "--json"])?;
    if !output.success {
        anyhow::bail!("could not list installed plugins: {}", output.failure());
    }
    let value: Value = serde_json::from_str(&output.stdout)
        .with_context(|| format!("{program} returned invalid plugin JSON"))?;
    Ok(find_plugin(&value))
}

#[derive(Debug, PartialEq)]
struct InstalledPlugin {
    version: Option<String>,
}

impl InstalledPlugin {
    fn matches(&self, expected: &str) -> bool {
        self.version.as_deref().and_then(normalize_version) == Some(expected)
    }

    fn display(&self) -> &str {
        self.version.as_deref().unwrap_or("unknown")
    }
}

fn find_plugin(value: &Value) -> Option<InstalledPlugin> {
    match value {
        Value::Array(items) => items.iter().find_map(find_plugin),
        Value::Object(object) => {
            let is_ortak = ["pluginId", "id"]
                .iter()
                .filter_map(|key| object.get(*key).and_then(Value::as_str))
                .any(|id| id == PLUGIN_ID)
                && object.get("installed").and_then(Value::as_bool) != Some(false);
            if is_ortak {
                return Some(InstalledPlugin {
                    version: object
                        .get("version")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            object.values().find_map(find_plugin)
        }
        _ => None,
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
    fn opencode_plugin_version(&mut self) -> Result<Option<String>>;
    fn install_opencode_plugin(&mut self, executable: &Path) -> Result<()>;
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

    fn opencode_plugin_version(&mut self) -> Result<Option<String>> {
        crate::opencode::installed_version()
    }

    fn install_opencode_plugin(&mut self, executable: &Path) -> Result<()> {
        let output = self.output(executable.as_os_str(), &["opencode", "install"])?;
        if !output.stdout.is_empty() {
            print!("{}", output.stdout);
        }
        if !output.stderr.is_empty() {
            eprint!("{}", output.stderr);
        }
        if !output.success {
            anyhow::bail!("`ortak opencode install` failed: {}", output.failure());
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
            "installed": [{"pluginId": "ortak@ortak", "version": "0.1.0"}]
        });
        let claude = serde_json::json!([
            {"id": "something@else"},
            {"id": "ortak@ortak", "version": "v0.2.0"}
        ]);
        assert_eq!(
            find_plugin(&codex),
            Some(InstalledPlugin {
                version: Some("0.1.0".to_string())
            })
        );
        assert!(find_plugin(&claude).unwrap().matches("0.2.0"));
        assert_eq!(find_plugin(&serde_json::json!({"installed": []})), None);
        assert_eq!(
            find_plugin(&serde_json::json!({
                "available": [{"pluginId": "ortak@ortak", "installed": false}]
            })),
            None
        );
        assert_eq!(normalize_version(" v0.2.0 "), Some("0.2.0"));
        assert_eq!(normalize_version("v bad"), None);
    }

    #[test]
    fn updates_the_current_binary_and_both_installed_plugins() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.responses = VecDeque::from([
            release(NEWER_VERSION),
            binary_version(NEWER_VERSION),
            codex_plugin(CURRENT_VERSION),
            ok("codex updated\n"),
            codex_plugin(NEWER_VERSION),
            claude_plugin(CURRENT_VERSION),
            ok("marketplace updated\n"),
            ok("plugin updated\n"),
            claude_plugin(NEWER_VERSION),
        ]);

        run_with(&mut runtime, Path::new("/opt/ortak/bin/ortak")).unwrap();

        assert_eq!(runtime.installed_in, Some(PathBuf::from("/opt/ortak/bin")));
        assert_eq!(
            &runtime.calls[1..],
            [
                "/opt/ortak/bin/ortak --version",
                "codex plugin list --json",
                "codex plugin marketplace upgrade ortak",
                "codex plugin list --json",
                "claude plugin list --json",
                "claude plugin marketplace update ortak",
                "claude plugin update --yes ortak@ortak",
                "claude plugin list --json",
            ]
        );
        assert_latest_call(&runtime.calls[0]);
    }

    #[test]
    fn skips_every_component_that_already_has_the_latest_version() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.responses = VecDeque::from([
            release(CURRENT_VERSION),
            codex_plugin(CURRENT_VERSION),
            claude_plugin(CURRENT_VERSION),
        ]);

        run_with(&mut runtime, Path::new("/opt/ortak/bin/ortak")).unwrap();

        assert_eq!(runtime.installed_in, None);
        assert_latest_call(&runtime.calls[0]);
        assert_eq!(
            &runtime.calls[1..],
            ["codex plugin list --json", "claude plugin list --json"]
        );
    }

    #[test]
    fn a_current_binary_does_not_hide_an_outdated_plugin() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.responses = VecDeque::from([
            release(CURRENT_VERSION),
            codex_plugin("0.1.0"),
            ok("codex updated\n"),
            codex_plugin(CURRENT_VERSION),
            claude_plugin(CURRENT_VERSION),
        ]);

        run_with(&mut runtime, Path::new("/opt/ortak/bin/ortak")).unwrap();

        assert_eq!(runtime.installed_in, None);
        assert_eq!(
            &runtime.calls[1..],
            [
                "codex plugin list --json",
                "codex plugin marketplace upgrade ortak",
                "codex plugin list --json",
                "claude plugin list --json",
            ]
        );
    }

    #[test]
    fn updates_an_installed_opencode_plugin_with_the_current_binary() {
        let mut runtime = Fake::with_programs(&["opencode"]);
        runtime.responses = VecDeque::from([release(CURRENT_VERSION)]);
        runtime.opencode_version = Some("0.1.0".to_string());
        runtime.opencode_after_install = Some(CURRENT_VERSION.to_string());

        run_with(&mut runtime, Path::new("/opt/ortak/bin/ortak")).unwrap();

        assert_latest_call(&runtime.calls[0]);
        assert_eq!(
            &runtime.calls[1..],
            ["/opt/ortak/bin/ortak opencode install"]
        );
        assert_eq!(runtime.opencode_version.as_deref(), Some(CURRENT_VERSION));
    }

    #[test]
    fn one_failure_does_not_prevent_the_other_updates() {
        let mut runtime = Fake::with_programs(&["codex", "claude"]);
        runtime.install_error = true;
        runtime.responses = VecDeque::from([
            release(NEWER_VERSION),
            codex_plugin(CURRENT_VERSION),
            failed("codex failed"),
            claude_plugin(CURRENT_VERSION),
            ok("marketplace updated\n"),
            ok("plugin updated\n"),
            claude_plugin(NEWER_VERSION),
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

    fn assert_latest_call(call: &str) {
        assert!(call.starts_with("curl --proto =https --tlsv1.2 -fsSL"));
        assert!(call.ends_with(LATEST_RELEASE_URL));
    }

    const NEWER_VERSION: &str = "999.0.0";

    fn release(version: &str) -> CommandOutput {
        ok(&format!(r#"{{"tag_name":"v{version}"}}"#))
    }

    fn binary_version(version: &str) -> CommandOutput {
        ok(&format!("ortak {version}\n"))
    }

    fn codex_plugin(version: &str) -> CommandOutput {
        ok(&format!(
            r#"{{"installed":[{{"pluginId":"ortak@ortak","version":"{version}"}}]}}"#
        ))
    }

    fn claude_plugin(version: &str) -> CommandOutput {
        ok(&format!(
            r#"[{{"id":"ortak@ortak","version":"{version}"}}]"#
        ))
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
        opencode_version: Option<String>,
        opencode_after_install: Option<String>,
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
                opencode_version: None,
                opencode_after_install: None,
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

        fn opencode_plugin_version(&mut self) -> Result<Option<String>> {
            Ok(self.opencode_version.clone())
        }

        fn install_opencode_plugin(&mut self, executable: &Path) -> Result<()> {
            self.calls
                .push(format!("{} opencode install", executable.display()));
            self.opencode_version = self.opencode_after_install.clone();
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
