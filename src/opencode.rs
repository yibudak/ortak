use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const PLUGIN_TEMPLATE: &str = include_str!("../plugins/ortak/opencode/ortak.js");
const SKILL: &str = include_str!("../plugins/ortak/skills/ortak-workflow/SKILL.md");
const VERSION_PLACEHOLDER: &str = "__ORTAK_VERSION__";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

struct Paths {
    plugin: PathBuf,
    skill: PathBuf,
    marker: PathBuf,
}

pub fn install() -> Result<()> {
    let paths = paths()?;
    install_to(&paths)?;
    println!(
        "OpenCode plugin and skill installed at:\n  {}\n  {}",
        paths.plugin.display(),
        paths.skill.display()
    );
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let paths = paths()?;
    if uninstall_from(&paths)? {
        println!("OpenCode plugin and skill removed.");
    } else {
        println!("Ortak is not installed in OpenCode; skipping it.");
    }
    Ok(())
}

pub fn installed_version() -> Result<Option<String>> {
    installed_version_at(&paths()?)
}

fn paths() -> Result<Paths> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .context("HOME and XDG_CONFIG_HOME are both unset; cannot locate OpenCode config")?;
    let root = config.join("opencode");
    let skill_dir = root.join("skills/ortak-workflow");
    Ok(Paths {
        plugin: root.join("plugins/ortak.js"),
        skill: skill_dir.join("SKILL.md"),
        marker: skill_dir.join(".ortak-version"),
    })
}

fn install_to(paths: &Paths) -> Result<()> {
    let managed = paths.marker.try_exists()?;
    let occupied = paths.plugin.try_exists()? || paths.skill.try_exists()?;
    if occupied && !managed {
        anyhow::bail!(
            "refusing to replace an unmanaged OpenCode plugin or skill; move {} and {} out of the way first",
            paths.plugin.display(),
            paths.skill.display()
        );
    }

    // Write the marker first. If a later write is interrupted, the next install
    // recognizes the partial installation as ours and can repair it.
    atomic_write(&paths.marker, format!("{CURRENT_VERSION}\n").as_bytes())?;
    let plugin = PLUGIN_TEMPLATE.replace(VERSION_PLACEHOLDER, CURRENT_VERSION);
    atomic_write(&paths.plugin, plugin.as_bytes())?;
    atomic_write(&paths.skill, SKILL.as_bytes())?;
    Ok(())
}

fn uninstall_from(paths: &Paths) -> Result<bool> {
    let managed = paths.marker.try_exists()?;
    let occupied = paths.plugin.try_exists()? || paths.skill.try_exists()?;
    if !managed {
        if occupied {
            anyhow::bail!(
                "refusing to remove an unmanaged OpenCode plugin or skill at {} and {}",
                paths.plugin.display(),
                paths.skill.display()
            );
        }
        return Ok(false);
    }

    // Keep the ownership marker until the managed files are gone. If a removal
    // fails, the next uninstall can safely recognize and finish the partial
    // cleanup instead of mistaking it for user-owned content.
    for path in [&paths.plugin, &paths.skill] {
        if path.try_exists()? {
            fs::remove_file(path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
    }
    fs::remove_file(&paths.marker)
        .with_context(|| format!("could not remove {}", paths.marker.display()))?;
    if let Some(skill_dir) = paths.marker.parent() {
        fs::remove_dir(skill_dir).ok();
    }
    Ok(true)
}

fn installed_version_at(paths: &Paths) -> Result<Option<String>> {
    let plugin = paths.plugin.try_exists()?;
    let skill = paths.skill.try_exists()?;
    let marker = paths.marker.try_exists()?;
    if !plugin && !skill && !marker {
        return Ok(None);
    }
    if !marker {
        anyhow::bail!(
            "OpenCode has an unmanaged or partial Ortak installation; run `ortak opencode install` after moving conflicting files"
        );
    }
    if !plugin || !skill {
        anyhow::bail!(
            "OpenCode's Ortak installation is incomplete; repair it with `ortak opencode install`"
        );
    }
    let version = fs::read_to_string(&paths.marker)
        .with_context(|| format!("could not read {}", paths.marker.display()))?;
    let version = version.trim();
    if version.is_empty()
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
    {
        anyhow::bail!("OpenCode's Ortak version marker is invalid");
    }
    Ok(Some(version.to_string()))
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut temporary = None;
    for attempt in 0..10 {
        let candidate = parent.join(format!(
            ".{name}.ortak-{}-{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("could not stage OpenCode integration"),
        }
    }
    let (temporary, mut file) = temporary.context("could not allocate an OpenCode staging file")?;
    let result = (|| -> Result<()> {
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
            .with_context(|| format!("could not replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        fs::remove_file(&temporary).ok();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_paths(name: &str) -> (PathBuf, Paths) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ortak-opencode-{name}-{}-{nonce}",
            std::process::id()
        ));
        let skill_dir = root.join("skills/ortak-workflow");
        let paths = Paths {
            plugin: root.join("plugins/ortak.js"),
            skill: skill_dir.join("SKILL.md"),
            marker: skill_dir.join(".ortak-version"),
        };
        (root, paths)
    }

    #[test]
    fn installs_a_versioned_plugin_and_skill() {
        let (root, paths) = test_paths("install");
        install_to(&paths).unwrap();

        let plugin = fs::read_to_string(&paths.plugin).unwrap();
        assert!(plugin.starts_with(&format!("// ortak-version: {CURRENT_VERSION}")));
        assert!(!plugin.contains(VERSION_PLACEHOLDER));
        assert_eq!(fs::read_to_string(&paths.skill).unwrap(), SKILL);
        assert_eq!(
            installed_version_at(&paths).unwrap(),
            Some(CURRENT_VERSION.to_string())
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn refuses_to_replace_an_unmanaged_plugin() {
        let (root, paths) = test_paths("unmanaged");
        fs::create_dir_all(paths.plugin.parent().unwrap()).unwrap();
        fs::write(&paths.plugin, "user plugin\n").unwrap();

        let error = install_to(&paths).unwrap_err().to_string();
        assert!(error.contains("unmanaged"));
        assert_eq!(fs::read_to_string(&paths.plugin).unwrap(), "user plugin\n");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn uninstalls_only_a_marker_owned_plugin_and_skill() {
        let (root, paths) = test_paths("uninstall");
        install_to(&paths).unwrap();
        let sibling = root.join("skills/keep/SKILL.md");
        fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        fs::write(&sibling, "keep\n").unwrap();

        assert!(uninstall_from(&paths).unwrap());
        assert!(!paths.plugin.exists());
        assert!(!paths.skill.exists());
        assert!(!paths.marker.exists());
        assert_eq!(fs::read_to_string(&sibling).unwrap(), "keep\n");
        assert!(!uninstall_from(&paths).unwrap());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn refuses_to_remove_an_unmanaged_opencode_file() {
        let (root, paths) = test_paths("uninstall-unmanaged");
        fs::create_dir_all(paths.plugin.parent().unwrap()).unwrap();
        fs::write(&paths.plugin, "user plugin\n").unwrap();

        let error = uninstall_from(&paths).unwrap_err().to_string();
        assert!(error.contains("unmanaged"));
        assert_eq!(fs::read_to_string(&paths.plugin).unwrap(), "user plugin\n");
        fs::remove_dir_all(root).ok();
    }
}
