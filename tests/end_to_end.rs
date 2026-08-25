//! ortak, driven the way a person drives it.
//!
//! Every other test in this crate calls a function with a temporary database.
//! That is how `ortak tell` shipped and then reached no working session for
//! three releases, and how the impact scan at publish shipped and then printed
//! nothing for two: both features were whole in their unit tests and dead in
//! the tool, and nothing in `cargo test` had ever run the tool.
//!
//! So this builds a git repository, starts the daemon, registers two sessions
//! through the real hooks, and reads back what a person would have read.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Cargo builds the binary for this test and hands over the path, so the thing
/// under test is the thing that ships.
const ORTAK: &str = env!("CARGO_BIN_EXE_ortak");

/// The ceiling on any one wait. The daemon watches the filesystem and answers
/// when it answers, so nothing here sleeps a fixed amount and hopes; every wait
/// polls a condition and returns the moment it holds. The ceiling is generous
/// because a loaded CI runner is slower than the laptop this was written on,
/// and only a genuine failure ever pays it.
const PATIENCE: Duration = Duration::from_secs(60);

/// The pair of files every scenario starts from: one defines a function, the
/// other calls it. That is the shape of the collision this tool exists for.
const LIB: &str = "pub fn remote_for(cfg: &Config) -> String {\n    cfg.remote.clone()\n}\n";
const CALLER: &str = "use crate::lib::remote_for;\n\nfn go(cfg: &Config) {\n    let r = remote_for(cfg);\n    println!(\"{}\", r);\n}\n";

/// One scenario: nothing, or why the tool did not do what it says it does.
type Scenario = fn() -> Result<(), String>;

#[test]
fn ortak_works_end_to_end() {
    // One after another, never at once: one daemon per workspace is the tool's
    // own rule, and cargo would otherwise run these on parallel threads. Each
    // scenario reports rather than panics, so one dead feature does not hide
    // the other scenarios.
    let scenarios: [(&str, Scenario); 8] = [
        (
            "an edit is credited to the session that made it",
            attribution,
        ),
        (
            "the gate denies an overlapping edit and allows a distant one",
            gate,
        ),
        (
            "the global orchestrator can allow an overlapping edit",
            global_orchestrator,
        ),
        (
            "a published branch carries this session's work and not the other's",
            publish_separates,
        ),
        (
            "a message reaches the other session before its next tool call",
            tell_arrives,
        ),
        (
            "publishing names the caller of what the branch changed",
            publish_names_the_caller,
        ),
        (
            "an error lands on the author, and the author hears about it",
            blame_reaches_the_author,
        ),
        (
            "doctor clears a workspace that can publish, and exits zero",
            doctor_clears_a_healthy_workspace,
        ),
    ];

    let total = scenarios.len();
    let mut broken = Vec::new();
    for (what, scenario) in scenarios {
        match scenario() {
            Ok(()) => println!("ok    {what}"),
            Err(why) => {
                println!("FAIL  {what}\n      {why}");
                broken.push(what);
            }
        }
    }
    assert!(
        broken.is_empty(),
        "{} of {total} end-to-end scenarios failed:\n  {}",
        broken.len(),
        broken.join("\n  ")
    );
}

/// The journal names the session that made the change, and not the human the
/// daemon falls back to when nobody can be named.
fn attribution() -> Result<(), String> {
    let ws = Live::start("attribution")?;
    let a = ws.join("agent-a")?;
    ws.edit(&a, "lib.rs", "cfg.remote.clone()", "cfg.remote.to_string()")?;

    let log = ws.ortak(&["log"]);
    if !log.contains(&format!("({})", a.label)) {
        return Err(format!(
            "`ortak log` does not credit {} with lib.rs:\n{log}",
            a.label
        ));
    }
    Ok(())
}

/// The gate is about lines, not files: the session that is somewhere else in
/// the workspace has to be able to work.
fn gate() -> Result<(), String> {
    let ws = Live::start("gate")?;
    let a = ws.join("agent-a")?;
    let b = ws.join("agent-b")?;
    ws.edit(&a, "lib.rs", "cfg.remote.clone()", "cfg.remote.to_string()")?;

    let denied = ws.hook(
        "pre-edit",
        &ws.edit_call(&b, "lib.rs", "cfg.remote", "cfg.host"),
    );
    if !denied.contains("\"deny\"") {
        return Err(format!(
            "the gate let {} into {}'s live region:\n{denied}",
            b.label, a.label
        ));
    }
    if !denied.contains(&a.label) {
        return Err(format!(
            "the denial does not say who owns the lines:\n{denied}"
        ));
    }

    let allowed = ws.hook(
        "pre-edit",
        &ws.edit_call(&b, "caller.rs", "let r =", "let remote ="),
    );
    if allowed.contains("permissionDecision") {
        return Err(format!(
            "the gate denied {} a file nobody holds:\n{allowed}",
            b.label
        ));
    }
    Ok(())
}

/// A global arbiter setting reaches the hook, and a workspace that does not
/// mention the orchestrator inherits it.
fn global_orchestrator() -> Result<(), String> {
    let ws = Live::start_with_allowing_arbiter("global-orchestrator")?;
    let a = ws.join("agent-a")?;
    let b = ws.join("agent-b")?;
    ws.edit(&a, "lib.rs", "cfg.remote.clone()", "cfg.remote.to_string()")?;

    let allowed = ws.hook(
        "pre-edit",
        &ws.edit_call(&b, "lib.rs", "cfg.remote", "cfg.host"),
    );
    if allowed.contains("\"deny\"") {
        return Err(format!(
            "the global arbiter allowed the edit, but the hook denied it:\n{allowed}"
        ));
    }
    Ok(())
}

/// Two sessions in one working tree, and a branch that holds one of them.
fn publish_separates() -> Result<(), String> {
    let ws = Live::start("publish")?;
    let a = ws.join("agent-a")?;
    let b = ws.join("agent-b")?;
    ws.edit(&a, "lib.rs", "cfg.remote.clone()", "cfg.remote.to_string()")?;
    ws.edit(
        &b,
        "caller.rs",
        "let r = remote_for(cfg);",
        "let r = remote_for(cfg); // checked",
    )?;

    let published = ws.ortak(&["publish", &a.label, "--branch", "t/a"]);
    let carried = run(&ws.root, "git", &["diff", "--name-only", "main", "t/a"])?;
    let carried: Vec<&str> = carried.split_whitespace().collect();
    if carried != ["lib.rs"] {
        return Err(format!(
            "the branch should carry lib.rs alone, and carries {carried:?}\npublish said:\n{published}"
        ));
    }
    Ok(())
}

/// What one session tells another has to arrive before that session's next tool
/// call. Waiting for a prompt is waiting for something an agent working through
/// a brief does once, at the start.
fn tell_arrives() -> Result<(), String> {
    let ws = Live::start("tell")?;
    let a = ws.join("agent-a")?;
    let b = ws.join("agent-b")?;

    let note = "remote_for takes a timeout now";
    ws.ortak(&["tell", &b.label, note, "--from", &a.label]);

    let before_edit = ws.hook(
        "pre-edit",
        &ws.edit_call(&b, "caller.rs", "let r =", "let remote ="),
    );
    if !before_edit.contains(note) {
        return Err(format!(
            "{} sent {} a message and it was not there before {}'s next edit. Delivery rides \
             PreToolUse; if it waits for a prompt instead, it waits for something an agent \
             working through a brief does once:\n{before_edit}",
            a.label, b.label, b.label
        ));
    }
    if before_edit.contains("\"deny\"") {
        return Err(format!(
            "delivering the message denied the edit:\n{before_edit}"
        ));
    }
    Ok(())
}

/// The break the gate cannot see: a signature this branch changed, and the call
/// site another session is sitting in. Publish is where it gets said, because
/// it is the last moment before the work becomes somebody else's problem.
fn publish_names_the_caller() -> Result<(), String> {
    let ws = Live::start("impact")?;
    let a = ws.join("agent-a")?;
    let b = ws.join("agent-b")?;
    ws.edit(
        &a,
        "lib.rs",
        "pub fn remote_for(cfg: &Config) -> String {",
        "pub fn remote_for(cfg: &Config, timeout: u64) -> String {",
    )?;
    ws.edit(
        &b,
        "caller.rs",
        "let r = remote_for(cfg);",
        "let r = remote_for(cfg); // checked",
    )?;

    let published = ws.ortak(&["publish", &a.label, "--branch", "t/a"]);
    if !published.contains("remote_for is referenced in caller.rs") {
        return Err(format!(
            "the publish did not name the caller of the function it changed. The scan reads this \
             session's live regions, so it has to run before the publish frees them:\n{published}"
        ));
    }
    if !published.contains(&b.label) {
        return Err(format!(
            "the publish named the file but not who is in it:\n{published}"
        ));
    }
    Ok(())
}

/// A workspace with a repository, a commit, the base branch, a remote and a
/// running daemon is one that publishes, so `doctor` has to say so and exit
/// zero. The broken workspaces are unit-tested against a temporary directory;
/// what only the real binary can show is the command being wired up at all, and
/// the exit code, which is the half a script reads.
fn doctor_clears_a_healthy_workspace() -> Result<(), String> {
    let ws = Live::start("doctor")?;
    // Nothing pushes here. What the check reads is that ortak's remote is a
    // remote this clone actually has.
    run(
        &ws.root,
        "git",
        &["remote", "add", "origin", &ws.root.display().to_string()],
    )?;

    let report = run(&ws.root, ORTAK, &["doctor"])
        .map_err(|e| format!("doctor refused a workspace that publishes fine:\n{e}"))?;
    if !report.contains("this workspace can publish") {
        return Err(format!(
            "doctor did not clear a healthy workspace:\n{report}"
        ));
    }
    let json = run(&ws.root, ORTAK, &["doctor", "--json"])?;
    if !json.contains("\"can_publish\": true") {
        return Err(format!(
            "--json disagrees with what a person is told:\n{json}"
        ));
    }
    Ok(())
}

/// A build error names where a failure surfaced, which is the file of whoever
/// ran the build. Blame has to reach past that to the session whose change
/// caused it, and then that session has to hear about it without being asked.
fn blame_reaches_the_author() -> Result<(), String> {
    let ws = Live::start("blame")?;
    let a = ws.join("agent-a")?;
    let b = ws.join("agent-b")?;
    ws.edit(
        &a,
        "lib.rs",
        "pub fn remote_for(cfg: &Config) -> String {",
        "pub fn remote_for(cfg: &Config, timeout: u64) -> String {",
    )?;
    ws.edit(
        &b,
        "caller.rs",
        "let r = remote_for(cfg);",
        "let r = remote_for(cfg); // checked",
    )?;

    // Real rustc: the call site is named first and the definition only in a
    // note, so the reporter's own file is the loudest thing in the output.
    let stopped = ws.ortak(&[
        "report",
        &b.label,
        "--command",
        "cargo test",
        "error[E0061]: this function takes 2 arguments but 1 argument was supplied\n \
         --> caller.rs:4:13\n  |\n4 |     let r = remote_for(cfg);\n  |             ^^^^^^^^^^ \
         argument #2 of type `u64` is missing\n  |\nnote: function defined here\n --> lib.rs:1:8",
    ]);
    if !stopped.contains(&format!("responsible: {}", a.label)) {
        return Err(format!(
            "the error came out of {}'s change and was not assigned to it:\n{stopped}",
            a.label
        ));
    }

    let told = ws.hook(
        "pre-edit",
        &ws.edit_call(&a, "lib.rs", "cfg.remote.clone()", "cfg.remote.to_string()"),
    );
    if !told.contains("YOU own the fix") {
        return Err(format!(
            "{} owns the stopped line and its next edit did not say so:\n{told}",
            a.label
        ));
    }
    if told.contains("\"deny\"") {
        return Err(format!(
            "the owner of the fix was denied the edit that fixes it:\n{told}"
        ));
    }
    Ok(())
}

/// One registered session: what the harness calls it, and what ortak calls it.
struct Agent {
    external: String,
    label: String,
}

/// A workspace with a daemon running in it. Everything is torn down on the way
/// out however the scenario ends, because a leaked daemon goes on watching a
/// deleted directory and the next run inherits it.
struct Live {
    root: PathBuf,
    home: PathBuf,
    daemon: Child,
}

impl Live {
    fn start(tag: &str) -> Result<Live, String> {
        Self::start_inner(tag, false)
    }

    fn start_with_allowing_arbiter(tag: &str) -> Result<Live, String> {
        Self::start_inner(tag, true)
    }

    fn start_inner(tag: &str, allowing_arbiter: bool) -> Result<Live, String> {
        let root = std::env::temp_dir().join(format!("ortak-e2e-{}-{tag}", std::process::id()));
        let home =
            std::env::temp_dir().join(format!("ortak-e2e-home-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&root).map_err(|e| format!("could not make {}: {e}", root.display()))?;
        fs::create_dir_all(&home).map_err(|e| format!("could not make {}: {e}", home.display()))?;
        fs::write(root.join("lib.rs"), LIB).map_err(|e| format!("could not write lib.rs: {e}"))?;
        fs::write(root.join("caller.rs"), CALLER)
            .map_err(|e| format!("could not write caller.rs: {e}"))?;

        if allowing_arbiter {
            let arbiter = home.join("allow-arbiter");
            fs::write(
                &arbiter,
                "#!/bin/sh\nprintf '%s\\n' '{\"decision\":\"allow\",\"message\":\"independent\"}'\n",
            )
            .map_err(|e| format!("could not write {}: {e}", arbiter.display()))?;
            let mut permissions = fs::metadata(&arbiter)
                .map_err(|e| e.to_string())?
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&arbiter, permissions).map_err(|e| e.to_string())?;
            let global_dir = home.join(".ortak");
            fs::create_dir_all(&global_dir).map_err(|e| e.to_string())?;
            fs::write(
                global_dir.join("config.toml"),
                format!(
                    "[orchestrator]\nenabled = true\ncommand = \"{}\"\nmodel = \"test\"\ntimeout_secs = 5\n",
                    arbiter.display()
                ),
            )
            .map_err(|e| e.to_string())?;
        }

        let git = |args: &[&str]| run(&root, "git", args);
        git(&["init", "-q", "-b", "main", "."])?;
        git(&["add", "-A"])?;
        git(&[
            "-c",
            "user.email=e2e@ortak.test",
            "-c",
            "user.name=ortak end to end",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-qm",
            "baseline",
        ])?;
        run_with_home(&root, &home, ORTAK, &["init"])?;
        // #63 declines a report naming a file another session wrote in the last
        // ninety seconds, which in a test that runs in four is every file, so
        // the stop-the-line scenario would never reach the assignment it is
        // about. Turning the window off keeps this test measuring one thing.
        // A no-op until #63 lands, since the key is not in the file before it.
        let cfg = root.join("ortak.toml");
        let text = std::fs::read_to_string(&cfg)
            .map_err(|e| e.to_string())?
            .replace("mid_write_seconds = 90", "mid_write_seconds = 0");
        std::fs::write(&cfg, text).map_err(|e| e.to_string())?;

        let daemon = Command::new(ORTAK)
            .arg("daemon")
            .current_dir(&root)
            .env("HOME", &home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start the daemon: {e}"))?;
        let live = Live { root, home, daemon };
        live.wait("the daemon to come up", || {
            live.ortak(&["status"]).contains("daemon: running")
        })?;
        Ok(live)
    }

    /// Register a session the way SessionStart does, and take its ortak-N out of
    /// what the hook tells the agent.
    fn join(&self, external: &str) -> Result<Agent, String> {
        let hello = self.hook(
            "session-start",
            &serde_json::json!({"cwd": self.root.display().to_string(), "session_id": external})
                .to_string(),
        );
        let label = hello
            .split("to ortak-")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .filter(|digits| !digits.is_empty())
            .map(|digits| format!("ortak-{digits}"))
            .ok_or_else(|| {
                format!("SessionStart did not tell {external} which session it is:\n{hello}")
            })?;
        self.ortak(&["intent", &label, "working"]);
        Ok(Agent {
            external: external.to_string(),
            label,
        })
    }

    /// The hook JSON Claude Code sends for one Edit call.
    fn edit_call(&self, who: &Agent, file: &str, from: &str, to: &str) -> String {
        serde_json::json!({
            "cwd": self.root.display().to_string(),
            "session_id": who.external,
            "tool_name": "Edit",
            "tool_input": {
                "file_path": self.root.join(file).display().to_string(),
                "old_string": from,
                "new_string": to,
            },
        })
        .to_string()
    }

    /// One edit, through both doors and onto the disk between them, then wait
    /// for the daemon to have it. This is what the harness does, in order.
    fn edit(&self, who: &Agent, file: &str, from: &str, to: &str) -> Result<(), String> {
        let call = self.edit_call(who, file, from, to);
        let gate = self.hook("pre-edit", &call);
        if gate.contains("\"deny\"") {
            return Err(format!(
                "the gate denied {}'s edit to {file}:\n{gate}",
                who.label
            ));
        }
        let path = self.root.join(file);
        let before =
            fs::read_to_string(&path).map_err(|e| format!("could not read {file}: {e}"))?;
        if !before.contains(from) {
            return Err(format!("{file} does not contain {from:?}"));
        }
        fs::write(&path, before.replacen(from, to, 1))
            .map_err(|e| format!("could not write {file}: {e}"))?;
        self.hook("post-edit", &call);
        self.wait(
            &format!("{file} to reach the journal against {}", who.label),
            || {
                self.ortak(&["log"])
                    .lines()
                    .any(|line| line.contains(file) && line.contains(&format!("({})", who.label)))
            },
        )
    }

    /// One ortak command. Both streams, because a test that drops stderr
    /// reports the wrong thing the moment a command fails.
    fn ortak(&self, args: &[&str]) -> String {
        match run_with_home(&self.root, &self.home, ORTAK, args) {
            Ok(text) | Err(text) => text,
        }
    }

    /// One hook, fed on stdin the way the harness feeds it.
    fn hook(&self, event: &str, payload: &str) -> String {
        let mut child = Command::new(ORTAK)
            .args(["hook", event])
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("could not run the {event} hook: {e}"));
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(payload.as_bytes())
            .unwrap_or_else(|e| panic!("could not feed the {event} hook: {e}"));
        let out = child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("the {event} hook did not finish: {e}"));
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    fn wait(&self, what: &str, holds: impl Fn() -> bool) -> Result<(), String> {
        let deadline = Instant::now() + PATIENCE;
        while !holds() {
            if Instant::now() >= deadline {
                return Err(format!(
                    "waited {}s for {what} and it never happened",
                    PATIENCE.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // Ask first: a daemon that detached is no longer this child, and the
        // one thing this test must never do is leave one running.
        let _ = Command::new(ORTAK)
            .args(["daemon", "--stop"])
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    run_command(Command::new(program), dir, program, args)
}

fn run_with_home(dir: &Path, home: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.env("HOME", home);
    run_command(command, dir, program, args)
}

fn run_command(
    mut command: Command,
    dir: &Path,
    program: &str,
    args: &[&str],
) -> Result<String, String> {
    let out = command
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("`{program} {}` failed:\n{text}", args.join(" ")))
    }
}
