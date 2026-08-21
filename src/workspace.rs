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
