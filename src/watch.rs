//! `ortak watch`: the second window on a workspace.
//!
//! A loop around queries `status` and `log` already run, with a filter in
//! front of them. The filter is the deliverable. This replaces a shell script
//! the watcher of this project rewrote in every round since round 7, and two
//! of those rounds went on tightening it after benign events kept firing:
//! **silence is the product**, and a monitor that prints continuously is a
//! worse `ortak log`, because `ortak log` at least does not interrupt.
//!
//! What it says a line about, and nothing else: a write that crosses sessions,
//! a write that landed on the person, a write the daemon refused to attribute,
//! the daemon stopping, the daemon running a binary that is not on disk any
//! more, a file the journal is failing on, and the line stopping or opening
//! again.

use crate::db::{self, Attribution, Db, EditRow, ErrorRow, JournalFailure};
use crate::workspace::Workspace;
use anyhow::Result;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

/// Journal rows read per tick. The first tick spends them learning who owns
/// what, since the owner map has to be built before it is consulted.
///
/// ponytail: a ceiling rather than a cursor. More rows than this inside one
/// interval and the oldest of them go unjudged; at 45 seconds that is four
/// agents writing faster than any of them can think.
const TICK_ROWS: u32 = 200;

/// One line the watch is prepared to interrupt somebody with.
#[derive(Debug, PartialEq)]
pub struct Event {
    pub ts: i64,
    pub tag: &'static str,
    pub text: String,
}

impl Event {
    fn at(ts: i64, tag: &'static str, text: String) -> Event {
        Event { ts, tag, text }
    }
}

/// Everything one tick reads, so the filter can be tried without a database,
/// a daemon or a clock.
pub struct Tick {
    pub now: i64,
    /// Journal rows oldest first. Newest first is right for a person reading
    /// `log` and wrong here: the owner map would be consulted before it was
    /// built and the cross-session test would never fire.
    pub edits: Vec<EditRow>,
    pub human: i64,
    /// Active sessions that are not the person. A row landing on the person
    /// is only news while somebody else is writing too.
    pub agents: usize,
    pub daemon_running: bool,
    pub heartbeat_age: Option<i64>,
    /// The path the daemon started from, when that is no longer the file
    /// there.
    pub stale_build: Option<String>,
    pub failing: Vec<JournalFailure>,
    pub open_errors: Vec<ErrorRow>,
}

/// What the watch carries from one tick to the next, which is what keeps it
/// quiet: every event below is a change rather than a state.
#[derive(Default)]
pub struct Seen {
    /// File to the session that last wrote it through a hook, which is the
    /// only attribution nobody had to guess at.
    owner: HashMap<String, String>,
    last_edit: i64,
    down: bool,
    stale_build: bool,
    failing: BTreeSet<String>,
    errors: BTreeSet<i64>,
}

impl Seen {
    /// Read the backlog without saying anything about it. Whatever happened
    /// before the watch started is history, and a monitor that opens with
    /// forty lines of it is one nobody leaves running.
    pub fn prime(&mut self, t: &Tick) {
        for e in &t.edits {
            self.last_edit = self.last_edit.max(e.id);
            self.note_owner(e);
        }
    }

    pub fn events(&mut self, t: &Tick, all: bool) -> Vec<Event> {
        let mut out = Vec::new();
        let after = self.last_edit;
        for e in t.edits.iter().filter(|e| e.id > after) {
            out.extend(self.judge(e, t, all));
        }
        if let Some(newest) = t.edits.last() {
            self.last_edit = self.last_edit.max(newest.id);
        }
        out.extend(self.health(t));
        out
    }

    /// A row marked `hook` is the file's owner from here on. `claim` and
    /// `contested` deliberately do not update it: those are the daemon's
    /// guesses, and a guess that overwrites the owner map is how the next
    /// crossing goes unnoticed.
    fn note_owner(&mut self, e: &EditRow) {
        let guessed = matches!(
            e.attributed_by.as_deref(),
            Some(a) if a == Attribution::Claim.as_str() || a == Attribution::Contested.as_str()
        );
        if !guessed {
            self.owner.insert(e.file.clone(), e.agent_name.clone());
        }
    }

    fn judge(&mut self, e: &EditRow, t: &Tick, all: bool) -> Option<Event> {
        let what = format!(
            "{} {} - {} (ortak-{})",
            e.change_kind, e.file, e.agent_name, e.session_id
        );
        let marker = e.attributed_by.as_deref();
        if marker == Some(Attribution::Contested.as_str()) {
            return Some(Event::at(
                e.ts,
                "contested",
                format!(
                    "{what}: the daemon would not name an owner, so the write went to the person"
                ),
            ));
        }
        // The rule that survived five rounds of tuning. Flagging every
        // claim-attributed write fired three times in a row on sessions
        // reformatting their own files; every real misattribution in twelve
        // rounds is one where the file's previous owner was somebody else.
        if marker == Some(Attribution::Claim.as_str()) {
            let before = self.owner.get(&e.file).filter(|o| *o != &e.agent_name);
            return match before {
                Some(other) => Some(Event::at(
                    e.ts,
                    "crossed",
                    format!(
                        "{what}: inferred from a running command, and {other} wrote this file last"
                    ),
                )),
                None => all.then(|| Event::at(e.ts, "edit", what)),
            };
        }
        self.note_owner(e);
        if e.session_id == t.human && t.agents > 0 {
            return Some(Event::at(
                e.ts,
                "person",
                format!(
                    "{what}: nothing claimed it, and {} agent session(s) are working here",
                    t.agents
                ),
            ));
        }
        all.then(|| Event::at(e.ts, "edit", what))
    }

    /// The daemon is the one recovery worth a line of its own: everything else
    /// in ortak keeps reporting normally while it is down, so a reader who
    /// missed the restart cannot tell from anywhere else. The rest are
    /// reported once, when they appear, and clear in silence.
    fn health(&mut self, t: &Tick) -> Vec<Event> {
        let mut out = Vec::new();
        match (self.down, t.daemon_running) {
            (false, false) => {
                self.down = true;
                let age = match t.heartbeat_age {
                    Some(age) => format!("last heartbeat {age}s ago"),
                    None => "it has never started here".to_string(),
                };
                out.push(Event::at(
                    t.now,
                    "daemon",
                    format!("NOT RUNNING ({age}); nothing written now is reaching the journal"),
                ));
            }
            (true, true) => {
                self.down = false;
                out.push(Event::at(
                    t.now,
                    "daemon",
                    "running again; `ortak status` says what the gap cost".to_string(),
                ));
            }
            _ => {}
        }
        match &t.stale_build {
            Some(path) if !self.stale_build => {
                self.stale_build = true;
                out.push(Event::at(
                    t.now,
                    "daemon",
                    format!("running a build that is no longer at {path}; the hooks read ortak from PATH on every call, so two builds are writing this journal"),
                ));
            }
            None => self.stale_build = false,
            _ => {}
        }
        for f in &t.failing {
            if self.failing.insert(f.file.clone()) {
                out.push(Event::at(
                    f.ts,
                    "journal",
                    format!(
                        "NOT RECORDING {} ({} in a row): {}",
                        f.file, f.streak, f.reason
                    ),
                ));
            }
        }
        self.failing
            .retain(|f| t.failing.iter().any(|now| &now.file == f));
        let open: BTreeSet<i64> = t.open_errors.iter().map(|e| e.id).collect();
        for e in &t.open_errors {
            if self.errors.insert(e.id) {
                out.push(Event::at(
                    e.ts_opened,
                    "stopped",
                    format!(
                        "error {} from {}, {} owes the fix: {}",
                        e.id,
                        e.reporter_name,
                        e.responsible_name(),
                        excerpt(&e.excerpt)
                    ),
                ));
            }
        }
        for id in self.errors.difference(&open).copied().collect::<Vec<_>>() {
            self.errors.remove(&id);
            out.push(Event::at(
                t.now,
                "moving",
                format!("error {id} is closed; the line is open again"),
            ));
        }
        out
    }
}

/// One line of somebody's error output, short enough to sit in a monitor.
fn excerpt(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(90) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_string(),
    }
}

/// Open, read, close. The daemon is writing this file, and a reader that holds
/// it open for hours is a new thing in this codebase; `busy_timeout` covers the
/// moment each tick is inside.
fn read(ws: &Workspace) -> Result<Tick> {
    let db = Db::open(&ws.db_path)?;
    let mut edits = db.recent_edits(None, TICK_ROWS)?;
    edits.reverse();
    let sessions = db.list_sessions()?;
    let age = db.heartbeat_age()?;
    let running = age.is_some_and(|a| a <= db::HEARTBEAT_ALIVE_SECS);
    Ok(Tick {
        now: db::now_ts(),
        edits,
        human: sessions
            .iter()
            .find(|s| s.kind == "human")
            .map_or(0, |s| s.id),
        agents: sessions
            .iter()
            .filter(|s| s.kind != "human" && s.status == "active")
            .count(),
        daemon_running: running,
        heartbeat_age: age,
        // A build only matters while something is running it, and a stopped
        // daemon has a louder line of its own.
        stale_build: running
            .then(|| crate::daemon::running_build(&db))
            .flatten()
            .and_then(|(build, current)| (!current).then_some(build.path)),
        failing: db.journal_failures()?,
        open_errors: db.open_errors()?,
    })
}

pub fn run(ws: &Workspace, interval: u64, all: bool) -> Result<()> {
    let interval = interval.max(1);
    println!(
        "watching {} every {}s. Silence means nothing worth a line. Ctrl-C to stop.",
        ws.root.display(),
        interval
    );
    let mut seen = Seen::default();
    let mut tick = read(ws)?;
    seen.prime(&tick);
    loop {
        for e in seen.events(&tick, all) {
            say(&e);
        }
        std::thread::sleep(Duration::from_secs(interval));
        // A tick that cannot read says so and waits for the next one. The
        // database going away under a watch is a reset, and a watch that dies
        // of that is one somebody has to remember to restart.
        tick = match read(ws) {
            Ok(t) => t,
            Err(e) => {
                say(&Event::at(
                    db::now_ts(),
                    "watch",
                    format!("cannot read the workspace: {e:#}"),
                ));
                continue;
            }
        };
    }
}

fn say(e: &Event) {
    println!(
        "[{}] {:<9} {}",
        db::fmt_local(e.ts, "%H:%M:%S"),
        e.tag,
        e.text
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(id: i64, session: i64, agent: &str, file: &str, by: Option<Attribution>) -> EditRow {
        EditRow {
            id,
            session_id: session,
            agent_name: agent.to_string(),
            file: file.to_string(),
            change_kind: "modify".to_string(),
            shadow_commit: None,
            ts: 1_700_000_000 + id,
            attributed_by: by.map(Attribution::as_str).map(str::to_string),
        }
    }

    fn tick(edits: Vec<EditRow>) -> Tick {
        Tick {
            now: 1_700_000_100,
            edits,
            human: 1,
            agents: 2,
            daemon_running: true,
            heartbeat_age: Some(3),
            stale_build: None,
            failing: Vec::new(),
            open_errors: Vec::new(),
        }
    }

    fn tags(events: &[Event]) -> Vec<&str> {
        events.iter().map(|e| e.tag).collect()
    }

    /// The rule the shell version took five rounds to arrive at. Both writes
    /// below are the daemon guessing from a command that was running; only one
    /// of them is somebody's work landing on another session.
    #[test]
    fn a_guessed_write_is_news_only_when_it_crosses_a_session() {
        let mut seen = Seen::default();
        seen.prime(&tick(vec![
            edit(1, 3, "claude-a", "src/db.rs", Some(Attribution::Hook)),
            edit(2, 4, "claude-b", "src/publish.rs", Some(Attribution::Hook)),
        ]));

        let events = seen.events(
            &tick(vec![
                // Its own file, reformatted by its own command: three of these
                // in a row are what rewrote the filter mid-round.
                edit(3, 4, "claude-b", "src/publish.rs", Some(Attribution::Claim)),
                // Somebody else's file.
                edit(4, 4, "claude-b", "src/db.rs", Some(Attribution::Claim)),
            ]),
            false,
        );
        assert_eq!(tags(&events), vec!["crossed"]);
        assert!(
            events[0].text.contains("src/db.rs") && events[0].text.contains("claude-a"),
            "the line names the file and who wrote it last: {}",
            events[0].text
        );
    }

    /// A guess must not become the owner: if it did, the session that took the
    /// file would look like its author and the next crossing would go unsaid.
    #[test]
    fn a_guess_does_not_become_the_owner_of_the_file() {
        let mut seen = Seen::default();
        seen.prime(&tick(vec![edit(
            1,
            3,
            "claude-a",
            "src/db.rs",
            Some(Attribution::Hook),
        )]));
        let first = seen.events(
            &tick(vec![edit(
                2,
                4,
                "claude-b",
                "src/db.rs",
                Some(Attribution::Claim),
            )]),
            false,
        );
        let again = seen.events(
            &tick(vec![edit(
                3,
                4,
                "claude-b",
                "src/db.rs",
                Some(Attribution::Claim),
            )]),
            false,
        );
        assert_eq!(tags(&first), vec!["crossed"]);
        assert_eq!(tags(&again), vec!["crossed"], "still claude-a's file");
    }

    /// An unclaimed write goes to the person, which is worth saying while
    /// agents are working and is just somebody typing when they are not.
    #[test]
    fn a_write_landing_on_the_person_is_news_only_beside_agents() {
        let mut seen = Seen::default();
        let row = edit(1, 1, "human", "src/main.rs", None);
        assert_eq!(
            tags(&seen.events(&tick(vec![row.clone()]), false)),
            ["person"]
        );

        let mut alone = Seen::default();
        let t = Tick {
            agents: 0,
            ..tick(vec![row])
        };
        assert!(alone.events(&t, false).is_empty(), "nobody else is here");
    }

    /// A contested row is the daemon refusing to choose, and it always needs a
    /// person: it is the one attribution nobody can check afterwards.
    #[test]
    fn a_contested_write_always_gets_a_line() {
        let mut seen = Seen::default();
        let t = Tick {
            agents: 0,
            ..tick(vec![edit(
                1,
                1,
                "human",
                "src/db.rs",
                Some(Attribution::Contested),
            )])
        };
        assert_eq!(tags(&seen.events(&t, false)), ["contested"]);
    }

    /// Down once, back once, and nothing in between. The shell version's most
    /// valuable line was this one, twice in six rounds.
    #[test]
    fn the_daemon_stopping_is_one_line_and_so_is_it_starting() {
        let mut seen = Seen::default();
        let down = || Tick {
            daemon_running: false,
            heartbeat_age: Some(90),
            ..tick(Vec::new())
        };
        assert!(seen.events(&tick(Vec::new()), false).is_empty());
        assert_eq!(tags(&seen.events(&down(), false)), ["daemon"]);
        assert!(
            seen.events(&down(), false).is_empty(),
            "still down is not an event"
        );
        assert_eq!(tags(&seen.events(&tick(Vec::new()), false)), ["daemon"]);
        assert!(seen.events(&tick(Vec::new()), false).is_empty());
    }

    /// `--all` is the way to check what the filter is hiding, which is the one
    /// thing a reader cannot do from the outside.
    #[test]
    fn all_prints_the_rows_the_filter_holds_back() {
        let rows = vec![
            edit(1, 3, "claude-a", "src/db.rs", Some(Attribution::Hook)),
            edit(2, 3, "claude-a", "src/db.rs", Some(Attribution::Claim)),
        ];
        assert!(Seen::default()
            .events(&tick(rows.clone()), false)
            .is_empty());
        assert_eq!(
            tags(&Seen::default().events(&tick(rows), true)),
            ["edit", "edit"]
        );
    }
}
