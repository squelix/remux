use anyhow::Result;
use async_trait::async_trait;
#[cfg(unix)]
use libc;
use std::{collections::HashSet, sync::Arc};
use tracing::{debug, info, warn};

use super::{ProgressReporter, Task, TaskCategory, TaskService};
use crate::{AppContext, torrent::PRECACHE_STORE_PREFIX};

pub struct CleanTranscodeFolderTask;

#[async_trait]
impl Task for CleanTranscodeFolderTask {
    fn key(&self) -> &str {
        "CleanTranscodeFolder"
    }
    fn name(&self) -> &str {
        "Clean Transcode Folder"
    }
    fn description(&self) -> &str {
        "Deletes temporary files left over from transcoding sessions."
    }
    fn short_description(&self) -> &str {
        "Deletes leftover temp transcode files"
    }
    fn category(&self) -> TaskCategory {
        TaskCategory::Maintenance
    }

    async fn run(
        &self,
        ctx: AppContext,
        _tasks: Arc<TaskService>,
        progress: ProgressReporter,
    ) -> Result<()> {
        let active: HashSet<String> = ctx
            .sessions
            .active_session_ids()
            .into_iter()
            .collect();
        let base = ctx
            .sessions
            .base_dir();
        let mut removed = 0usize;

        for entry in super::iter_dir(base) {
            let name = entry
                .file_name()
                .to_string_lossy()
                .into_owned();
            if !active.contains(&name) {
                // Kill any orphaned ffmpeg process before removing the dir.
                #[cfg(unix)]
                if let Ok(pid_str) = std::fs::read_to_string(
                    entry
                        .path()
                        .join(".pid"),
                ) {
                    if let Ok(pid) = pid_str
                        .trim()
                        .parse::<libc::pid_t>()
                    {
                        if pid > 0 {
                            unsafe {
                                libc::kill(pid, libc::SIGCONT);
                                libc::kill(pid, libc::SIGKILL);
                            }
                        }
                    }
                }
                if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                    warn!(
                        "failed to remove transcode dir {}: {e:#}",
                        entry
                            .path()
                            .display()
                    );
                } else {
                    removed += 1;
                }
            }
        }
        info!(removed, "cleaned orphaned transcode dirs");

        progress.set(50.0);

        // Collect torrent IDs currently being streamed by active sessions so we
        // don't pull the rug out from under an in-progress playback.
        let mut active_torrent_ids = HashSet::new();
        for session in ctx
            .sessions
            .get_all()
        {
            if let Some(tc) = session.transcode {
                let input_url = tc
                    .read()
                    .await
                    .input_url
                    .clone();
                if let Some(id) =
                    crate::torrent::TorrentManager::torrent_id_from_url(&input_url)
                {
                    active_torrent_ids.insert(id);
                }
            }
        }

        // Also exempt torrents recently precached for an upcoming episode. A
        // precached torrent belongs to the *next* episode, which nobody is
        // actively playing yet, so it never shows up in `active_torrent_ids`
        // above — without this, this sweep could delete a precached-but-not-
        // yet-played torrent (and its downloaded head pieces) out from under
        // the feature, silently defeating it. See `precache_store_key`.
        let precache_exempt = precached_torrent_ids(&ctx.store);
        debug!(exempt = precache_exempt.len(), "precache-exempt torrents");
        active_torrent_ids.extend(precache_exempt);

        let deleted = ctx
            .torrent
            .delete_unused_with_files(&active_torrent_ids)
            .await
            .unwrap_or_else(|e| {
                warn!("failed to clean torrents: {e:#}");
                0
            });
        info!(deleted, "cleaned torrent sessions");

        progress.set(100.0);
        Ok(())
    }
}

/// Torrent IDs currently marked "recently precached" (see
/// `torrent::precache_store_key`), exempt from deletion for the marker's
/// grace window.
fn precached_torrent_ids(store: &remux_utils::Store) -> HashSet<usize> {
    store
        .scan_keys(PRECACHE_STORE_PREFIX)
        .into_iter()
        .filter_map(|key| {
            key.strip_prefix(PRECACHE_STORE_PREFIX)?
                .parse()
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn precached_torrent_ids_reads_back_marked_ids() {
        let store = remux_utils::Store::new(100);
        store.save(
            crate::torrent::precache_store_key(7),
            (),
            Duration::from_secs(3600),
        );
        store.save(
            crate::torrent::precache_store_key(9),
            (),
            Duration::from_secs(3600),
        );

        let ids = precached_torrent_ids(&store);
        assert_eq!(ids, HashSet::from([7, 9]));
    }

    #[test]
    fn precached_torrent_ids_ignores_unrelated_keys() {
        let store = remux_utils::Store::new(100);
        store.save(
            "pstream:some-item:device",
            uuid::Uuid::nil(),
            Duration::from_secs(3600),
        );

        assert!(precached_torrent_ids(&store).is_empty());
    }
}
