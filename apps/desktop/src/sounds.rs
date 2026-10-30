//! Sound packs from folders on this computer.
//!
//! A pack is a folder of `<sound>.wav` files named after the sound ids in
//! the UI's sounds.js (e.g. `user-joined.wav`). Such packs stay local: they
//! are not part of the app and are never uploaded.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::CmdResult;

#[derive(Serialize)]
pub struct LocalPack {
    /// The folder as given.
    path: String,
    /// The folder's name, shown in the pack list.
    name: String,
    /// Sound ids the folder has a file for.
    sounds: Vec<String>,
}

/// Only plain sound ids, so a file name never leaves the folder.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn sound_path(folder: &str, id: &str) -> Option<PathBuf> {
    valid_id(id).then(|| Path::new(folder).join(format!("{id}.wav")))
}

/// Looks into each folder; folders that cannot be read are skipped.
#[tauri::command]
pub fn local_sound_packs(folders: Vec<String>) -> Vec<LocalPack> {
    folders
        .into_iter()
        .filter_map(|path| {
            let entries = std::fs::read_dir(&path).ok()?;
            let mut sounds: Vec<String> = entries
                .filter_map(|e| {
                    let name = e.ok()?.file_name().into_string().ok()?;
                    let id = name.strip_suffix(".wav")?;
                    valid_id(id).then(|| id.to_owned())
                })
                .collect();
            sounds.sort();
            let name = Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            Some(LocalPack { path, name, sounds })
        })
        .collect()
}

/// The bytes of one sound of a local pack.
#[tauri::command]
pub fn local_sound(folder: String, id: String) -> CmdResult<tauri::ipc::Response> {
    let path = sound_path(&folder, &id).ok_or("invalid sound name")?;
    let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(tauri::ipc::Response::new(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_escape_the_folder() {
        assert!(sound_path("C:\\sounds", "user-joined").is_some());
        assert!(sound_path("C:\\sounds", "../secret").is_none());
        assert!(sound_path("C:\\sounds", "a/b").is_none());
        assert!(sound_path("C:\\sounds", "").is_none());
    }

    #[test]
    fn lists_only_sound_files() {
        let dir = std::env::temp_dir().join(format!("tinlink-sounds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["user-joined.wav", "notes.txt", "Bad Name.wav"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let packs = local_sound_packs(vec![dir.display().to_string(), "/does/not/exist".into()]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].sounds, ["user-joined"]);
    }
}
