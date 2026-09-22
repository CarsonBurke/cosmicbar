//! Brightness modes: named sets of per-display levels — `day`, `night` — that
//! one click in the popup, a right-click on the cell or
//! `cosmicbar brightness-mode [name]` applies.
//!
//! They live in `$XDG_CONFIG_HOME/cosmicbar/brightness-modes.toml`, beside the
//! config rather than inside it: the popup is the editor, and a bar rewriting
//! the hand-kept `config.toml` would throw away its comments and layout. The
//! file is still plain TOML, re-read on `cosmicbar reload`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    pub name: String,
    /// Percent per display, keyed by [`super::Found::key`]. A display the mode
    /// does not name keeps its level when the mode is applied, so a mode saved
    /// before a monitor was plugged in leaves that monitor alone.
    pub levels: BTreeMap<String, u32>,
}

impl Mode {
    /// `DP-1 80% · DP-2 70%`
    pub fn summary(&self) -> String {
        summary(self.levels.iter().map(|(key, level)| (key.as_str(), *level)))
    }
}

pub fn summary<'a>(levels: impl Iterator<Item = (&'a str, u32)>) -> String {
    levels
        .map(|(key, level)| format!("{key} {level}%"))
        .collect::<Vec<_>>()
        .join(" · ")
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, rename = "mode")]
    modes: Vec<Mode>,
}

const HEADER: &str = "\
# Brightness modes, edited from cosmicbar's brightness popup. Hand edits are
# picked up by `cosmicbar reload`. Levels are percent per display, keyed by
# connector (or backlight device).

";

/// Held across every read and write of the file; its value is the generation
/// of the newest save that has run, whether or not its write succeeded, see
/// [`save`]. A read therefore never lands between a save's write and its
/// rename, and says which saves the contents it read already account for.
static FILE: tokio::sync::Mutex<u64> = tokio::sync::Mutex::const_new(0);

/// What [`load`] read.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub modes: Vec<Mode>,
    /// The newest save the file had seen when it was read. Modes edited since
    /// — a save with a higher generation — are newer than these.
    pub generation: u64,
}

pub fn path() -> PathBuf {
    crate::config::Config::path().with_file_name("brightness-modes.toml")
}

/// The saved modes; none when the file does not exist yet.
pub async fn load() -> Result<Loaded, String> {
    let file = FILE.lock().await;
    let path = path();
    let modes = match tokio::fs::read_to_string(&path).await {
        Ok(text) => toml::from_str::<File>(&text)
            .map(|file| file.modes)
            .map_err(|error| format!("{}: {error}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    Ok(Loaded {
        modes,
        generation: *file,
    })
}

/// Write `modes` as the file's whole contents. `generation` orders the saves:
/// each edit spawns its own write, and two of them in flight must not land
/// oldest-last, so a save older than one that has already run is dropped —
/// even when that one failed, since what it would have written is newer.
pub async fn save(modes: Vec<Mode>, generation: u64) -> Result<(), String> {
    let mut newest = FILE.lock().await;
    if generation <= *newest {
        return Ok(());
    }
    *newest = generation;
    let path = path();
    write(&path, &File { modes })
        .await
        .map_err(|error| format!("{}: {error:#}", path.display()))
}

async fn write(path: &std::path::Path, file: &File) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let text = format!("{HEADER}{}", toml::to_string(file)?);
    // Write-and-rename, so a crash mid-write leaves the old file whole.
    let temporary = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, text).await?;
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_round_trips() {
        let file = File {
            modes: vec![
                Mode {
                    name: "day".into(),
                    levels: [("DP-1".into(), 80), ("DP-2".into(), 70)].into(),
                },
                Mode {
                    name: "late night".into(),
                    levels: [("DP-1".into(), 10)].into(),
                },
            ],
        };
        let text = format!("{HEADER}{}", toml::to_string(&file).unwrap());
        let read: File = toml::from_str(&text).unwrap();
        assert_eq!(read.modes, file.modes);
        assert!(text.contains("[[mode]]"), "{text}");
    }

    #[test]
    fn a_missing_table_is_no_modes() {
        let read: File = toml::from_str("").unwrap();
        assert!(read.modes.is_empty());
    }

    #[test]
    fn summaries_list_every_level() {
        let mode = Mode {
            name: "night".into(),
            levels: [("DP-1".into(), 20), ("DP-2".into(), 15)].into(),
        };
        assert_eq!(mode.summary(), "DP-1 20% · DP-2 15%");
    }
}
