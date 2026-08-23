//! Rationale notes: why a particular change was made.
//!
//! `ortak intent` records one sentence per session. Nothing recorded why any
//! single change was made, so an agent that opened a file and found something
//! surprising could learn who did it and when, and never why. Notes outlive the
//! session that wrote them, which is the whole point of keeping them.

use crate::db::Db;
use crate::regions::WHOLE_FILE;
use crate::workspace::Workspace;
use anyhow::{bail, Result};

/// `ortak why ortak-N <file> <text...>` writes; `ortak why <file>[:<line>]`
/// reads. One argument means a read, three or more mean a write.
pub fn run(args: &[String]) -> Result<()> {
    let ws = Workspace::discover_from_cwd()?;
    let db = Db::open(&ws.db_path)?;
    match args {
        [target] => read(&ws, &db, target),
        [session, file, text @ ..] if !text.is_empty() => {
            write(&ws, &db, session, file, &text.join(" "))
        }
        _ => bail!("usage: ortak why <session> <file> \"<why>\"  or  ortak why <file>[:<line>]"),
    }
}

fn write(ws: &Workspace, db: &Db, session_ref: &str, file: &str, text: &str) -> Result<()> {
    let session = db.resolve_session(session_ref)?;
    let rel = relativize(ws, file);
    // ponytail: the note is pinned to the region as it stood when written, and
    // nothing moves it afterwards, so a note can drift off the code it
    // describes. Region tracking for notes can come when notes exist to track.
    let (start, end) = match db.session_region(session.id, &rel)? {
        Some(r) => (r.start, r.end),
        None => (1, WHOLE_FILE),
    };
    db.insert_note(session.id, &rel, start, end, text)?;
    println!("noted on {} {}: {}", rel, range(start, end), text);
    Ok(())
}

fn read(ws: &Workspace, db: &Db, target: &str) -> Result<()> {
    let (file, line) = split_target(target);
    let rel = relativize(ws, file);
    let mut notes = db.file_notes(&rel)?;
    if let Some(line) = line {
        notes.retain(|n| n.start <= line && line <= n.end);
        if notes.is_empty() {
            println!("no note covers line {} of {}", line, rel);
            return Ok(());
        }
    } else if notes.is_empty() {
        println!("no notes on {}", rel);
        return Ok(());
    }
    println!("{}", rel);
    let now = crate::db::now_ts();
    for n in &notes {
        println!(
            "  {} - ortak-{} {}, {}",
            range(n.start, n.end),
            n.session_id,
            n.agent_name,
            ago(now - n.ts)
        );
        println!("    {}", n.text);
    }
    Ok(())
}

fn range(start: i64, end: i64) -> String {
    if end >= WHOLE_FILE {
        "whole file".to_string()
    } else if start == end {
        format!("line {}", start)
    } else {
        format!("lines {}-{}", start, end)
    }
}

/// Split `src/db.rs:143` into its file and line. Anything after the last colon
/// that is not a line number belongs to the filename.
fn split_target(target: &str) -> (&str, Option<i64>) {
    match target.rsplit_once(':') {
        Some((file, line)) => match line.parse::<i64>() {
            Ok(n) if n > 0 => (file, Some(n)),
            _ => (target, None),
        },
        None => (target, None),
    }
}

/// The journal keys files on their workspace-relative path, so an argument
/// typed from a subdirectory or as an absolute path has to be brought back to
/// that. A path from outside the workspace passes through and matches nothing.
fn relativize(ws: &Workspace, file: &str) -> String {
    let path = std::path::Path::new(file);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => return file.to_string(),
        }
    };
    ws.relativize(&abs).unwrap_or_else(|| file.to_string())
}

/// Rough age for someone reading a list: whichever unit keeps it short.
fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 90 => format!("{}s ago", s),
        s if s < 5400 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Note;
    use crate::regions::Hunk;

    fn covering(notes: &[Note], line: i64) -> Vec<&Note> {
        notes
            .iter()
            .filter(|n| n.start <= line && line <= n.end)
            .collect()
    }

    #[test]
    fn a_note_takes_the_range_its_session_owns() {
        let path = std::env::temp_dir().join(format!("ortak-why-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        let with = db
            .upsert_session("sess-with", "claude-aaaa", "llm", Some("claude-code"))
            .unwrap();
        let without = db
            .upsert_session("sess-without", "claude-bbbb", "llm", Some("claude-code"))
            .unwrap();

        // One session has edited the file and owns lines 10-14; the other has
        // not touched it at all.
        db.insert_edit(with, "src/publish.rs", "modify", None, &[])
            .unwrap();
        db.apply_edit_regions(
            with,
            "src/publish.rs",
            &[Hunk {
                old_start: 10,
                old_lines: 5,
                new_start: 10,
                new_lines: 5,
            }],
        )
        .unwrap();

        let region = db.session_region(with, "src/publish.rs").unwrap().unwrap();
        db.insert_note(with, "src/publish.rs", region.start, region.end, "seeded")
            .unwrap();
        assert!(db
            .session_region(without, "src/publish.rs")
            .unwrap()
            .is_none());
        db.insert_note(without, "src/publish.rs", 1, WHOLE_FILE, "no region here")
            .unwrap();

        let notes = db.file_notes("src/publish.rs").unwrap();
        assert_eq!(notes.len(), 2);

        // Line 12 is inside the region, so both the pinned note and the
        // whole-file one answer for it.
        let at_12 = covering(&notes, 12);
        assert_eq!(at_12.len(), 2);
        assert!(at_12.iter().any(|n| n.text == "seeded"));

        // Line 99 is outside it, so only the whole-file note does.
        let at_99 = covering(&notes, 99);
        assert_eq!(at_99.len(), 1);
        assert_eq!(at_99[0].text, "no region here");

        // A file whose only note is pinned answers for nothing outside it.
        db.insert_edit(with, "src/db.rs", "modify", None, &[])
            .unwrap();
        db.apply_edit_regions(
            with,
            "src/db.rs",
            &[Hunk {
                old_start: 10,
                old_lines: 5,
                new_start: 10,
                new_lines: 5,
            }],
        )
        .unwrap();
        let r = db.session_region(with, "src/db.rs").unwrap().unwrap();
        db.insert_note(with, "src/db.rs", r.start, r.end, "pinned")
            .unwrap();
        assert!(covering(&db.file_notes("src/db.rs").unwrap(), 99).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_trailing_line_number_is_not_part_of_the_filename() {
        assert_eq!(split_target("src/db.rs:143"), ("src/db.rs", Some(143)));
        assert_eq!(split_target("src/db.rs"), ("src/db.rs", None));
        assert_eq!(split_target("odd:name.rs"), ("odd:name.rs", None));
    }
}
