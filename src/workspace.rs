use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub const ORTAK_DIR: &str = ".ortak";
pub const CONFIG_FILE: &str = "ortak.toml";

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
}
