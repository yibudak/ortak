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

    let mut defs: Vec<(String, String)> = Vec::new(); // (name, the file defining it)
    for (file, start, end) in db.session_regions(me.id)? {
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

    // Only files another active session has recently touched can matter, so
    // search those rather than walking the whole workspace for names nobody
    // else is working near.
    let recent = db.recent_session_files(cfg.line.blame_lookback_minutes * 60)?;
    let mut hits = 0;
    for (name, defined_in) in &defs {
        let mut named = false;
        for (sid, agent, file, ts) in &recent {
            if *sid == me.id || file == defined_in {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(ws.root.join(file)) else {
                continue;
            };
            if !mentions(&text, name) {
                continue;
            }
            if !named {
                println!("  {} is referenced in {}", name, file);
                named = true;
            }
            hits += 1;
            let mins = ((crate::db::now_ts() - ts) / 60).max(0);
            let intent = db
                .get_session(*sid)
                .ok()
                .and_then(|s| s.task_intent)
                .unwrap_or_else(|| "(not reported)".to_string());
            println!(
                "    ortak-{} {} has edits there, {} min ago, intent: {}",
                sid, agent, mins, intent
            );
        }
    }
    if hits == 0 {
        println!("  no other active session has touched a file that mentions these.");
    }
    Ok(())
}

/// The name a line defines, if it looks like a definition. Covers Rust `fn`,
/// `struct`, `enum`, `trait`, `const`, `static` and `type`; Python `def` and
/// `class`; JavaScript `function`, `class` and `export const`.
fn defined_name(line: &str) -> Option<&str> {
    const KEYWORDS: [&str; 10] = [
        "fn", "struct", "enum", "trait", "const", "static", "type", "def", "class", "function",
    ];
    let trimmed = line.trim_start();
    // Comments and attributes define nothing, and both spell keywords freely.
    if ["//", "#", "*", "/*"]
        .iter()
        .any(|p| trimmed.starts_with(p))
    {
        return None;
    }
    let mut tokens = trimmed.split_whitespace();
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

/// Does `text` use `name` as a whole word? A substring match would report every
/// `run` in the workspace.
fn mentions(text: &str, name: &str) -> bool {
    let boundary = |c: char| !(c.is_alphanumeric() || c == '_');
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back().is_none_or(boundary);
        let after = text[at + name.len()..].chars().next().is_none_or(boundary);
        before && after
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
