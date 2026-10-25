//! On-disk storage of uploaded files, one file per attachment id.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use uuid::Uuid;

use crate::state::AppState;

pub fn path(state: &AppState, id: Uuid) -> PathBuf {
    state.config.files_dir.join(id.to_string())
}

/// Removes files older than `min_age` that have no attachment row any more,
/// e.g. after their channel or group was deleted.
pub async fn remove_unreferenced(state: &AppState, min_age: Duration) -> anyhow::Result<usize> {
    let mut dir = match tokio::fs::read_dir(&state.config.files_dir).await {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    let now = SystemTime::now();
    let mut candidates = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<Uuid>().ok())
        else {
            continue;
        };
        let modified = entry.metadata().await?.modified()?;
        if now.duration_since(modified).unwrap_or_default() >= min_age {
            candidates.push(id);
        }
    }
    if candidates.is_empty() {
        return Ok(0);
    }
    let known: HashSet<Uuid> = sqlx::query_scalar("SELECT id FROM attachments WHERE id = ANY($1)")
        .bind(&candidates)
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect();
    let mut removed = 0;
    for id in candidates.into_iter().filter(|id| !known.contains(id)) {
        tokio::fs::remove_file(path(state, id)).await?;
        removed += 1;
    }
    Ok(removed)
}
