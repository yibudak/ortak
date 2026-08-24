use crate::config::Config;
use crate::db::Db;
use crate::workspace::Workspace;
use anyhow::Result;

/// What else in this workspace might a session have broken?
///
/// The gate compares line regions, so it catches two sessions editing the same
/// lines and is blind to the commoner failure: one session changes a function's
/// signature at line 10 of one file while another calls it at line 200 of a
/// different file. Both edits pass, both branches are green alone, and the
/// breakage shows up only once both merge.
///
/// ponytail: name extraction is a scan for definition keywords, not a parser.
/// It misses names defined inside macros, generated code, re-exports and
/// anything dispatched dynamically, and it cannot tell a call from a comment
/// that happens to spell the name. A rename is reported under the new name, so
/// the callers of the old one are not found; a changed signature keeps its name
/// and is what this catches. A real version wants tree-sitter. A heuristic that
/// catches the common case beats nothing as long as it does not claim to be
/// complete.
pub fn run(ws: &Workspace, cfg: &Config, session_ref: &str) -> Result<()> {
    let db = Db::open(&ws.db_path)?;
    let me = db.resolve_session(session_ref)?;
    let (defs, refs) = scan(ws, cfg, &db, me.id)?;

    if defs.is_empty() {
        println!(
            "ortak-{} {} has no live regions that define anything ortak can name.",
            me.id, me.agent_name
        );
        return Ok(());
    }
    let changed: Vec<String> = defs.iter().map(|(n, f)| format!("{} ({})", n, f)).collect();
    println!(
        "ortak-{} {} changed: {}",
        me.id,
        me.agent_name,
        changed.join(", ")
    );
    if refs.is_empty() {
        println!("  no other active session has touched a file that mentions these.");
        return Ok(());
    }
    print_refs(&refs);
    println!(
        "names are matched as text, so the short and the everyday ones are left out; read the rest before believing them"
    );
    Ok(())
}

/// One place another session's work meets a name this session changed.
pub struct Ref {
    pub name: String,
    /// The file mentioning the name, never the one that defines it.
    pub file: String,
    pub session: i64,
    pub agent: String,
    pub minutes: i64,
    pub intent: String,
}

/// What a scan found: the (name, defining file) pairs this session's live
/// regions define, and the references other sessions make to them.
pub type Scan = (Vec<(String, String)>, Vec<Ref>);

/// Two rules keep the report readable, both of them about the same thing: a
/// text match on an ordinary English word is not evidence of anything.
///
/// A test file this project shipped defines `run`, `it`, `edit`, `start` and
/// `drop`, and the scan reported every session in every file that spells any of
/// them: fifty-odd lines, one of which was real. A report wrong that often is
/// not read a fourth time, which is worse than no report.
///
/// Shorter than four characters is never checked. It costs the odd real `run`,
/// and it buys back the half of the workspace that says "run the daemon" in a
/// comment.
const SHORTEST_NAME: usize = 4;
/// Matched in more than half the files searched, and never fewer than three, so
/// a quiet workspace does not filter on a sample of two. Half rather than a
/// fixed count on purpose: a real function called from five of twenty recent
/// files is the case this scan exists for, and a flat cap would hide it.
const FEWEST_FILES: usize = 3;

/// How many different files a run of rows names.
fn distinct<'a>(files: impl Iterator<Item = &'a str>) -> usize {
    files.collect::<std::collections::HashSet<_>>().len()
}

/// The names this session's live regions define, and every reference the other
/// sessions' recent files make to them. Two empty lists is the usual answer;
/// `publish` reports the second and stays quiet about the first.
pub fn scan(ws: &Workspace, cfg: &Config, db: &Db, session_id: i64) -> Result<Scan> {
    let mut defs: Vec<(String, String)> = Vec::new(); // (name, the file defining it)
    for (file, start, end) in db.session_regions(session_id)? {
        let Ok(text) = std::fs::read_to_string(ws.root.join(&file)) else {
            continue;
        };
        let skip = (start.max(1) - 1) as usize;
        let take = (end - start + 1).max(0) as usize;
        for line in text.lines().skip(skip).take(take) {
            if let Some(name) = defined_name(line) {
                let pair = (name.to_string(), file.clone());
                if !defs.contains(&pair) {
                    defs.push(pair);
                }
            }
        }
    }
    // Only files another active session has recently touched can matter, so
    // search those rather than walking the whole workspace for names nobody
    // else is working near.
    let recent = db.recent_session_files(cfg.line.blame_lookback_minutes * 60)?;
    let searched = distinct(
        recent
            .iter()
            .filter(|(sid, _, _, _)| *sid != session_id)
            .map(|(_, _, file, _)| file.as_str()),
    );
    let vocabulary = (searched / 2).max(FEWEST_FILES);
    // Rarest name first: the fewer files a name turns up in, the more likely it
    // is a symbol somebody actually calls, and the report is read from the top.
    let mut by_name: Vec<(usize, Vec<Ref>)> = Vec::new();
    for (name, defined_in) in &defs {
        if name.len() < SHORTEST_NAME {
            continue;
        }
        let mut hits: Vec<Ref> = Vec::new();
        for (sid, agent, file, ts) in &recent {
            if *sid == session_id || file == defined_in {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(ws.root.join(file)) else {
                continue;
            };
            if !mentions(&text, name) {
                continue;
            }
            hits.push(Ref {
                name: name.clone(),
                file: file.clone(),
                session: *sid,
                agent: agent.clone(),
                minutes: ((crate::db::now_ts() - ts) / 60).max(0),
                intent: db
                    .get_session(*sid)
                    .ok()
                    .and_then(|s| s.task_intent)
                    .unwrap_or_else(|| "(not reported)".to_string()),
            });
        }
        let files = distinct(hits.iter().map(|r| r.file.as_str()));
        if files > vocabulary {
            continue;
        }
        by_name.push((files, hits));
    }
    by_name.sort_by_key(|(files, _)| *files);
    let refs = by_name.into_iter().flat_map(|(_, hits)| hits).collect();
    Ok((defs, refs))
}

/// The references, one heading per name and file. `publish` prints the same
/// shape, so a session reads one report whichever command produced it.
pub fn print_refs(refs: &[Ref]) {
    let mut heading: Option<(&str, &str)> = None;
    for r in refs {
        if heading != Some((&r.name, &r.file)) {
            println!("  {} is referenced in {}", r.name, r.file);
            heading = Some((&r.name, &r.file));
        }
        println!(
            "    ortak-{} {} has edits there, {} min ago, intent: {}",
            r.session, r.agent, r.minutes, r.intent
        );
    }
}

/// The name a line defines, if it looks like a definition. Covers Rust `fn`,
/// `struct`, `enum`, `trait`, `const`, `static` and `type`; Python `def` and
/// `class`; JavaScript `function`, `class` and `export const`.
fn defined_name(line: &str) -> Option<&str> {
    const KEYWORDS: [&str; 10] = [
        "fn", "struct", "enum", "trait", "const", "static", "type", "def", "class", "function",
    ];
    if is_comment(line) {
        return None;
    }
    let mut tokens = line.split_whitespace();
    let name = loop {
        let token = tokens.next()?;
        if KEYWORDS.contains(&token) {
            break tokens.next()?;
        }
    };
    let ident: &str = name
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()?;
    if ident.is_empty() || KEYWORDS.contains(&ident) {
        return None;
    }
    Some(ident)
}

/// Does `text` use `name` as a whole word, somewhere that is not a comment?
///
/// A substring match would report every `run` in the workspace. A match in
/// prose is the same mistake one layer in: most of what this scan used to
/// print came from doc comments spelling `drop` or `start` in a sentence, and
/// a sentence calls nothing.
fn mentions(text: &str, name: &str) -> bool {
    text.lines()
        .filter(|line| !is_comment(line))
        .any(|line| whole_word(line, name))
}

fn whole_word(line: &str, name: &str) -> bool {
    let boundary = |c: char| !(c.is_alphanumeric() || c == '_');
    line.match_indices(name).any(|(at, _)| {
        let before = line[..at].chars().next_back().is_none_or(boundary);
        let after = line[at + name.len()..].chars().next().is_none_or(boundary);
        before && after
    })
}

/// Comments and attributes define nothing and call nothing, and both spell
/// keywords and names freely.
fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    ["//", "#", "*", "/*"]
        .iter()
        .any(|p| trimmed.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regions::Hunk;

    /// The scan `publish` runs: a name inside this session's live region, and
    /// another session working in a file that spells it.
    #[test]
    fn scan_finds_the_other_session_that_uses_the_name() {
        let dir = std::env::temp_dir().join(format!("ortak-impact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.rs"),
            "pub fn remote_for(cfg: &Config) {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("hooks.rs"), "let r = remote_for(&cfg);\n").unwrap();

        let ws = Workspace::at(&dir);
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let me = db
            .upsert_session("mine", "claude-mine", "llm", None)
            .unwrap();
        let them = db
            .upsert_session("theirs", "claude-theirs", "llm", None)
            .unwrap();
        let first_line = Hunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 1,
        };
        db.apply_edit_regions(me, "config.rs", &[first_line], None)
            .unwrap();
        db.insert_edit(them, "hooks.rs", "modify", None, &[], None)
            .unwrap();

        let (defs, refs) = scan(&ws, &Config::default(), &db, me).unwrap();
        assert_eq!(
            defs,
            vec![("remote_for".to_string(), "config.rs".to_string())]
        );
        assert_eq!(refs.len(), 1, "{:?}", refs.len());
        assert_eq!(refs[0].name, "remote_for");
        assert_eq!(refs[0].file, "hooks.rs");
        assert_eq!(refs[0].session, them);

        // A session with no regions defines nothing, so it breaks nobody.
        let (defs, refs) = scan(&ws, &Config::default(), &db, them).unwrap();
        assert!(defs.is_empty() && refs.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_definitions_out_of_rust() {
        assert_eq!(
            defined_name("pub fn remote_for(cfg: &Config) {"),
            Some("remote_for")
        );
        assert_eq!(
            defined_name("    pub(crate) async fn base_seed() {"),
            Some("base_seed")
        );
        assert_eq!(defined_name("pub struct Workspace {"), Some("Workspace"));
        assert_eq!(
            defined_name("pub const HINT_TTL_SECS: i64 = 15;"),
            Some("HINT_TTL_SECS")
        );
        assert_eq!(
            defined_name("type FreshRegion = (String, i64);"),
            Some("FreshRegion")
        );
    }

    #[test]
    fn reads_definitions_out_of_python_and_javascript() {
        assert_eq!(
            defined_name("def take_hint(self, path):"),
            Some("take_hint")
        );
        assert_eq!(
            defined_name("class Session(models.Model):"),
            Some("Session")
        );
        assert_eq!(
            defined_name("export const publish = () => {}"),
            Some("publish")
        );
    }

    #[test]
    fn lines_that_define_nothing_stay_quiet() {
        assert_eq!(defined_name("    let files = db.session_files(id)?;"), None);
        assert_eq!(defined_name("// fn remote_for is gone now"), None);
        assert_eq!(defined_name("#[derive(Debug)]"), None);
        assert_eq!(defined_name(""), None);
        assert_eq!(defined_name("        }"), None);
    }

    #[test]
    fn a_name_matches_only_as_a_whole_word() {
        assert!(mentions("let x = remote_for(&cfg);", "remote_for"));
        assert!(!mentions("let x = remote_format(&cfg);", "remote_for"));
        assert!(!mentions("let x = my_remote_for;", "remote_for"));
    }

    /// Prose is where a codebase spells its ordinary words, and prose calls
    /// nothing. Most of what this scan used to print came from doc comments.
    #[test]
    fn a_name_in_a_comment_is_not_a_reference() {
        assert!(!mentions("// drop the commit and keep replaying\n", "drop"));
        assert!(!mentions("    /// start of the region\n", "start"));
        assert!(!mentions("#[derive(Debug)] // start\n", "start"));
        assert!(mentions("// start here\nlet r = start(&tag);\n", "start"));
    }

    /// Round 7 published a test file defining `run`, `it`, `edit` and `drop`,
    /// and the scan answered with fifty lines naming every session in every
    /// file that spells any of them. One line of the fifty was real, and a
    /// report wrong that often stops being read.
    #[test]
    fn names_that_mean_nothing_stay_out_of_the_report() {
        let dir = std::env::temp_dir().join(format!("ortak-noise-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mine.rs"),
            "fn it(what: &str) {}\nfn edit(file: &str) {}\nfn helper() {}\nfn remote_for() {}\n",
        )
        .unwrap();

        let ws = Workspace::at(&dir);
        let db = Db::open(&dir.join("db.sqlite")).unwrap();
        let me = db.upsert_session("a", "claude-a", "llm", None).unwrap();
        let them = db.upsert_session("b", "claude-b", "llm", None).unwrap();
        // Eight files the other session is in. Every one of them spells `edit`
        // and `it`, the way a codebase spells its own vocabulary; three call
        // `helper`, and one calls `remote_for`.
        for n in 0..8 {
            let file = format!("other{n}.rs");
            let mut body = String::from("let it = edit(path);\n");
            if n < 3 {
                body.push_str("helper();\n");
            }
            if n == 0 {
                body.push_str("remote_for();\n");
            }
            std::fs::write(dir.join(&file), body).unwrap();
            db.insert_edit(them, &file, "modify", None, &[], None)
                .unwrap();
        }
        db.apply_edit_regions(
            me,
            "mine.rs",
            &[Hunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 4,
            }],
        )
        .unwrap();

        let (defs, refs) = scan(&ws, &Config::default(), &db, me).unwrap();
        assert_eq!(defs.len(), 4, "the session still changed all four names");
        let named: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(
            !named.contains(&"it"),
            "a two-letter name is not evidence: {named:?}"
        );
        assert!(
            !named.contains(&"edit"),
            "a name every file here spells is not evidence: {named:?}"
        );
        // Rarest first: the one file that calls remote_for leads, and helper's
        // three come after it.
        assert_eq!(named[0], "remote_for", "{named:?}");
        assert_eq!(named.len(), 4, "{named:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
