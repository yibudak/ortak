use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub const ORTAK_DIR: &str = ".ortak";
pub const CONFIG_FILE: &str = "ortak.toml";

/// How far under the root to look for repositories. A tree of them keeps them
/// one or two levels down; past three this is walking somebody's node_modules
/// hoping to find a stray `.git`.
const REPO_DEPTH: usize = 3;

/// Resolved paths of an ortak workspace.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub ortak_dir: PathBuf,
    pub db_path: PathBuf,
    pub shadow_dir: PathBuf,
    pub config_path: PathBuf,
}

impl Workspace {
    pub fn at(root: &Path) -> Self {
        let root = root.to_path_buf();
        let ortak_dir = root.join(ORTAK_DIR);
        Workspace {
            db_path: ortak_dir.join("db.sqlite"),
            shadow_dir: ortak_dir.join("shadow"),
            config_path: root.join(CONFIG_FILE),
            ortak_dir,
            root,
        }
    }

    /// Walk upward from `start` until a directory containing `.ortak` is found.
    pub fn discover(start: &Path) -> Result<Self> {
        let mut cur = Some(start.to_path_buf());
        while let Some(dir) = cur {
            if dir.join(ORTAK_DIR).is_dir() {
                return Ok(Workspace::at(&dir));
            }
            cur = dir.parent().map(|p| p.to_path_buf());
        }
        bail!(
            "ortak workspace not found (no .ortak directory at or above {}); run `ortak init` first",
            start.display()
        );
    }

    pub fn discover_from_cwd() -> Result<Self> {
        Workspace::discover(&std::env::current_dir()?)
    }

    /// A path as somebody typed it at the command line, in the form the journal
    /// stores: relative to the workspace root, from whatever directory the
    /// command was run in. `None` when it lands outside the workspace.
    ///
    /// `sub/../src/db.rs` and `./src/db.rs` are the same file as `src/db.rs`
    /// and none of them is the key the journal holds, so a command that looks
    /// the path up as typed answers that nobody has ever touched it.
    pub fn relativize_arg(&self, arg: &str) -> Option<String> {
        let path = Path::new(arg);
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(path)
        };
        // A file that is not on disk cannot be canonicalized, and a blamed or
        // released file may well have been deleted. The joined path is then the
        // best answer available, and it is the one these commands already gave.
        let abs = abs.canonicalize().unwrap_or(abs);
        self.relativize(&abs)
    }

    /// The git repository a workspace-relative path belongs to: the nearest
    /// directory at or above it that holds a `.git`, as a workspace-relative
    /// directory path. `Some("")` is the workspace root itself. `None` when
    /// nothing at or above the file inside the workspace is a repository.
    ///
    /// A workspace can hold sixty repositories under one root, and each of them
    /// is the only thing entitled to say what it tracks and what it ignores. An
    /// ancestor's `.gitignore` describes what *that* repository does not hold,
    /// which is why this answers with the nearest one rather than the outermost.
    ///
    /// `.git` is a file rather than a directory in a linked worktree and in a
    /// submodule, so this tests for existence. The walk stops at the workspace
    /// root and never goes above it: a repository containing the workspace has
    /// no say over a tree it was never told about.
    pub fn repo_of(&self, rel: &str) -> Option<String> {
        let mut parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
        loop {
            let dir = parts.join("/");
            if self.root.join(&dir).join(".git").exists() {
                return Some(dir);
            }
            parts.pop()?;
        }
    }

    /// Every git repository this workspace covers, workspace-relative and
    /// sorted, with `""` first when the root is one itself.
    ///
    /// The walk stops at each repository it finds, because whatever a
    /// repository holds is that repository's business, and it does not care
    /// whether a parent's `.gitignore` hides the directory: that is the whole
    /// point of a workspace laid over a tree of them.
    pub fn repositories(&self) -> Vec<String> {
        let mut found = Vec::new();
        if self.root.join(".git").exists() {
            found.push(String::new());
        }
        self.collect_repositories(&self.root, 0, &mut found);
        found.sort();
        found
    }

    fn collect_repositories(&self, dir: &Path, depth: usize, out: &mut Vec<String>) {
        if depth >= REPO_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == ".git" || name == ORTAK_DIR {
                continue;
            }
            // Not `is_dir`, which follows a symlink: a link back up the tree
            // would walk this in circles.
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let Some(rel) = self.relativize(&path) else {
                continue;
            };
            if path.join(".git").exists() {
                out.push(rel);
                continue;
            }
            self.collect_repositories(&path, depth + 1, out);
        }
    }

    /// Workspace-relative, forward-slash path for an absolute path inside the root.
    pub fn relativize(&self, abs: &Path) -> Option<String> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        let s = rel.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            return None;
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `blame` and `why` looked the argument up as typed, so a path that walked
    /// out of a directory and back in named a file the journal has never heard
    /// of, and both answered that nobody had touched it.
    #[test]
    fn a_path_that_walks_through_a_directory_is_still_the_file_it_names() {
        let root = std::env::temp_dir().join(format!("ortak-ws-arg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("src/db.rs"), "fn main() {}\n").unwrap();
        // The temp directory is reached through a symlink on macOS, and the
        // root a workspace records is the resolved one.
        let root = root.canonicalize().unwrap();
        let ws = Workspace::at(&root);
        let arg = |p: &str| ws.relativize_arg(root.join(p).to_str().unwrap());

        assert_eq!(arg("src/db.rs").as_deref(), Some("src/db.rs"));
        assert_eq!(arg("sub/../src/db.rs").as_deref(), Some("src/db.rs"));
        assert_eq!(arg("./src/db.rs").as_deref(), Some("src/db.rs"));
        // A file the journal remembers and the disk no longer has still
        // resolves, because a deleted file is a thing people blame.
        assert_eq!(arg("src/gone.rs").as_deref(), Some("src/gone.rs"));
        assert_eq!(ws.relativize_arg("/etc/hosts"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A workspace holding sixty repositories under one root has sixty
    /// different answers to "who says what happens to this file", and the
    /// nearest one is always the right one.
    #[test]
    fn the_nearest_repository_above_a_file_is_the_one_that_owns_it() {
        let outer = std::env::temp_dir().join(format!("ortak-ws-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outer);
        let root = outer.join("workspace");
        std::fs::create_dir_all(root.join("repos/inner/models")).unwrap();
        std::fs::create_dir_all(root.join("addons/sale")).unwrap();
        std::fs::create_dir_all(root.join("worktree")).unwrap();
        // A repository the workspace sits inside, which has no say over it.
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        let root = root.canonicalize().unwrap();
        let ws = Workspace::at(&root);

        // Nothing inside the workspace is a repository yet, and the one above
        // it does not count.
        assert_eq!(ws.repo_of("addons/sale/models.py"), None);
        assert_eq!(ws.repo_of(""), None);

        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("repos/inner/.git")).unwrap();
        // A linked worktree and a submodule keep a `.git` file rather than a
        // directory, and both are repositories.
        std::fs::write(root.join("worktree/.git"), "gitdir: /elsewhere\n").unwrap();

        assert_eq!(ws.repo_of("addons/sale/models.py").as_deref(), Some(""));
        assert_eq!(
            ws.repo_of("repos/inner/models/sale.py").as_deref(),
            Some("repos/inner")
        );
        assert_eq!(ws.repo_of("worktree/x.rs").as_deref(), Some("worktree"));
        // The repository root itself, and the directory above it, which the
        // root still owns.
        assert_eq!(ws.repo_of("repos/inner").as_deref(), Some("repos/inner"));
        assert_eq!(ws.repo_of("repos").as_deref(), Some(""));
        assert_eq!(ws.repo_of("").as_deref(), Some(""));

        // The same tree, counted rather than asked about one path: the root
        // first, then the two under it, and never the one above.
        assert_eq!(
            ws.repositories(),
            vec![
                "".to_string(),
                "repos/inner".to_string(),
                "worktree".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&outer);
    }
}
