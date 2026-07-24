use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use futures_util::{Stream, StreamExt};
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions,
    api::{Api, TorrentIdOrHash},
    http_api::HttpApi,
};
use remux_utils::Store;
use tracing::{debug, warn};

/// TTL for the "recently precached" marker (see `precache_store_key`). Long
/// enough to cover a realistic "watching episode N, will get to N+1
/// eventually" gap, short enough not to permanently exempt a torrent.
const PRECACHE_GRACE_TTL: Duration = Duration::from_secs(24 * 3600);

/// Store key prefix marking a torrent as recently precached for an upcoming
/// episode. `CleanTranscodeFolderTask` scans for this prefix to exempt
/// precached-but-not-yet-played torrents from its deletion sweep.
pub const PRECACHE_STORE_PREFIX: &str = "precached_torrent:";

/// Store key marking a torrent as recently precached for an upcoming episode.
pub fn precache_store_key(torrent_id: usize) -> String {
    format!("{PRECACHE_STORE_PREFIX}{torrent_id}")
}

pub struct TorrentManager {
    session: Arc<Session>,
    http_port: u16,
}

impl TorrentManager {
    pub async fn new(
        data_dir: PathBuf,
        http_port: Option<u16>,
        disable_dht: bool,
        peer_port: Option<u16>,
    ) -> Result<Self> {
        let session = Session::new_with_opts(
            data_dir,
            SessionOptions {
                disable_dht,
                disable_dht_persistence: disable_dht,
                listen_port_range: peer_port.map(|p| p..p + 10),
                ..Default::default()
            },
        )
        .await?;

        // None → let the OS pick a free ephemeral port.
        let bind_port = http_port.unwrap_or(0);
        let listener =
            tokio::net::TcpListener::bind(format!("127.0.0.1:{}", bind_port)).await?;

        let bound_port = listener
            .local_addr()?
            .port();

        let api = Api::new(session.clone(), None, None);
        let http_api = HttpApi::new(api, None);
        tokio::spawn(http_api.make_http_api_and_run(listener, None));

        debug!(port = bound_port, "torrent HTTP server listening");
        Ok(Self {
            session,
            http_port: bound_port,
        })
    }

    /// Gracefully shut down the librqbit session, releasing all sockets
    /// (including the DHT UDP socket). Call this before dropping the manager
    /// to avoid "address already in use" errors on restart.
    pub async fn shutdown(&self) {
        self.session
            .stop()
            .await;
    }

    /// Resolve a magnet URI (possibly with `&tr=`, `&file_idx=`, `&file=` params
    /// we encode) to a local `http://127.0.0.1:<port>/torrents/<id>/stream/<file_idx>` URL
    pub async fn resolve_url(&self, magnet: &str) -> Result<String> {
        let file_idx_override = parse_file_idx_param(magnet);
        let wanted_file = parse_file_param(magnet);
        debug!(
            magnet,
            ?wanted_file,
            ?file_idx_override,
            "resolving torrent"
        );

        let opts = wanted_file
            .as_deref()
            .map(|name| AddTorrentOptions {
                // Ask librqbit to download only the matching file so we don't pull the
                // whole torrent.  The regex is anchored at the end so "Movie.mkv" doesn't
                // match "Movie.mkv.nfo".
                only_files_regex: Some(format!("(?i){}$", regex::escape(name))),
                ..Default::default()
            });

        let response = self
            .session
            .add_torrent(AddTorrent::from_url(magnet), opts)
            .await
            .context("failed to add torrent")?;

        let (torrent_id, handle) = match response {
            AddTorrentResponse::Added(id, h) => (id, h),
            AddTorrentResponse::AlreadyManaged(id, h) => (id, h),
            AddTorrentResponse::ListOnly(_) => {
                anyhow::bail!("unexpected ListOnly response")
            }
        };

        tokio::time::timeout(Duration::from_secs(30), handle.wait_until_initialized())
            .await
            .context("timed out waiting for torrent metadata")?
            .context("torrent initialization failed")?;

        // file_idx from the magnet params takes precedence; fall back to name search.
        let file_idx = if let Some(idx) = file_idx_override {
            idx
        } else {
            handle
                .with_metadata(|meta| {
                    if let Some(name) = wanted_file.as_deref() {
                        meta.file_infos
                            .iter()
                            .enumerate()
                            .find(|(_, fi)| {
                                fi.relative_filename
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .map(|n| n.eq_ignore_ascii_case(name))
                                    .unwrap_or(false)
                            })
                            .map(|(idx, _)| idx)
                            .unwrap_or(0)
                    } else {
                        0
                    }
                })
                .unwrap_or(0)
        };

        Ok(format!(
            "http://127.0.0.1:{}/torrents/{}/stream/{}",
            self.http_port, torrent_id, file_idx
        ))
    }

    /// Pre-download roughly the first `max_bytes` of the resolved torrent file
    /// by issuing an open-ended Range read. librqbit prioritizes the pieces at
    /// the read position; consumed pieces persist to the torrent data dir so a
    /// later play reuses them. Best-effort.
    ///
    /// Bounded by `timeout`: unlike normal playback reads (torn down when the
    /// client disconnects), this stream has no consumer lifecycle, so a slow
    /// or seederless torrent could otherwise run forever. On timeout, whatever
    /// was downloaded so far is kept (it's already on disk) and returned as
    /// `Ok(partial_bytes)` rather than propagated as an error — consistent
    /// with the stream-error case below, which also logs-and-returns-partial.
    ///
    /// As soon as the torrent is added, marks it in `store` (see
    /// `precache_store_key`) so `CleanTranscodeFolderTask`'s maintenance sweep
    /// exempts it from deletion during the grace window between "precached"
    /// and "actually played" — this covers both the in-progress download and
    /// the wait afterwards.
    pub async fn precache_head(
        &self,
        magnet: &str,
        max_bytes: u64,
        timeout: Duration,
        store: &Store,
    ) -> Result<u64> {
        let url = self
            .resolve_url(magnet)
            .await
            .context("failed to resolve magnet for precache")?;

        if let Some(id) = Self::torrent_id_from_url(&url) {
            store.save(precache_store_key(id), (), PRECACHE_GRACE_TTL);
        }

        let resp = reqwest::Client::new()
            .get(&url)
            .header(reqwest::header::RANGE, "bytes=0-")
            .send()
            .await
            .context("precache request failed")?;

        let downloaded = Arc::new(AtomicU64::new(0));
        let stream = resp.bytes_stream();
        if tokio::time::timeout(
            timeout,
            stream_capped(stream, max_bytes, downloaded.clone()),
        )
        .await
        .is_err()
        {
            warn!(
                bytes = downloaded.load(Ordering::Relaxed),
                ?timeout,
                "precache timed out; keeping partial download"
            );
        }
        Ok(downloaded.load(Ordering::Relaxed))
    }

    /// Delete managed torrents and their files, skipping any whose ID is in `active`.
    pub async fn delete_unused_with_files(
        &self,
        active: &std::collections::HashSet<usize>,
    ) -> Result<usize> {
        let api = Api::new(
            self.session
                .clone(),
            None,
            None,
        );
        let ids: Vec<_> = api
            .api_torrent_list()
            .torrents
            .into_iter()
            .filter_map(|t| t.id)
            .filter(|id| !active.contains(id))
            .collect();
        let count = ids.len();
        for id in ids {
            if let Err(e) = api
                .api_torrent_action_delete(TorrentIdOrHash::Id(id))
                .await
            {
                warn!(id, "failed to delete torrent: {e:#}");
            }
        }
        Ok(count)
    }

    /// Parse the torrent ID out of a librqbit stream URL.
    /// Format: `http://127.0.0.1:{port}/torrents/{id}/stream/{file_idx}`
    pub fn torrent_id_from_url(url: &str) -> Option<usize> {
        let after_host = url
            .split_once("//")?
            .1
            .split_once('/')?
            .1;
        let mut parts = after_host.splitn(3, '/');
        if parts.next()? != "torrents" {
            return None;
        }
        parts
            .next()?
            .parse()
            .ok()
    }

    /// Apply upload/download speed limits.  0 = no limit (for download) or
    /// effectively-disabled (for upload — 1 bps is used since the API requires
    /// `NonZeroU32`).
    pub fn update_limits(&self, upload_kbps: i64, download_kbps: i64) {
        use std::num::NonZeroU32;
        // upload: 0 means "don't seed" — clamp to 1 bps (librqbit requires NonZero)
        let upload = NonZeroU32::new(if upload_kbps <= 0 {
            1
        } else {
            (upload_kbps as u32).saturating_mul(1024)
        });
        // download: 0 means unlimited → None
        let download = if download_kbps <= 0 {
            None
        } else {
            NonZeroU32::new((download_kbps as u32).saturating_mul(1024))
        };
        self.session
            .ratelimits
            .set_upload_bps(upload);
        self.session
            .ratelimits
            .set_download_bps(download);
    }
}

/// Consume `stream` chunk-by-chunk, tracking bytes read in `downloaded`, until
/// `max_bytes` is reached, the stream errors, or it ends. `downloaded` is an
/// `Arc` (rather than a plain return value) specifically so that a caller
/// racing this future against `tokio::time::timeout` can still read the
/// partial progress after the future is dropped on cancellation.
async fn stream_capped<S, E>(mut stream: S, max_bytes: u64, downloaded: Arc<AtomicU64>)
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    while downloaded.load(Ordering::Relaxed) < max_bytes {
        match stream
            .next()
            .await
        {
            Some(Ok(chunk)) => {
                downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            Some(Err(e)) => {
                warn!(
                    bytes = downloaded.load(Ordering::Relaxed),
                    "precache stream error: {e:#}"
                );
                break;
            }
            None => break,
        }
    }
}

/// Extract the `file=` query parameter we encode into our magnet URIs.
fn parse_file_param(magnet: &str) -> Option<String> {
    let query = magnet
        .split_once('?')?
        .1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "file")
        .map(|(_, v)| v.into_owned())
}

/// Extract the `file_idx=` query parameter we encode into our magnet URIs.
fn parse_file_idx_param(magnet: &str) -> Option<usize> {
    let query = magnet
        .split_once('?')?
        .1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "file_idx")
        .and_then(|(_, v)| {
            v.parse()
                .ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    /// Sanity check: consuming stops once `max_bytes` worth of chunks have
    /// been read, even if more chunks remain in the stream.
    #[tokio::test]
    async fn stream_capped_stops_at_max_bytes() {
        let chunks: Vec<std::result::Result<bytes::Bytes, std::io::Error>> = vec![
            Ok(bytes::Bytes::from_static(&[0u8; 10])),
            Ok(bytes::Bytes::from_static(&[0u8; 10])),
            Ok(bytes::Bytes::from_static(&[0u8; 10])),
        ];
        let downloaded = Arc::new(AtomicU64::new(0));
        stream_capped(stream::iter(chunks), 15, downloaded.clone()).await;
        // The cap is checked between chunks, not mid-chunk, so it can
        // overshoot slightly — matches the "roughly max_bytes" contract.
        assert_eq!(downloaded.load(Ordering::Relaxed), 20);
    }

    /// This is the core mechanism behind Finding 1's timeout fix: when the
    /// caller races `stream_capped` against `tokio::time::timeout` and the
    /// timeout wins, the `Arc<AtomicU64>` counter must still reflect whatever
    /// was downloaded before the future was dropped on cancellation.
    #[tokio::test]
    async fn stream_capped_reports_partial_progress_after_external_timeout() {
        // Yields one 5-byte chunk, then blocks forever (simulating a stalled
        // seederless torrent that never sends another chunk). `.boxed()`
        // erases the concrete (non-`Unpin`) `Unfold` future type so it
        // satisfies `stream_capped`'s `Unpin` bound, same as a real
        // `reqwest::Response::bytes_stream()` would.
        let stalled = stream::unfold(0u8, |state| async move {
            if state == 0 {
                Some((
                    Ok::<_, std::io::Error>(bytes::Bytes::from_static(&[0u8; 5])),
                    1,
                ))
            } else {
                futures_util::future::pending::<()>().await;
                unreachable!("pending future never resolves")
            }
        })
        .boxed();

        let downloaded = Arc::new(AtomicU64::new(0));
        let result = tokio::time::timeout(
            Duration::from_millis(50),
            stream_capped(stalled, 100, downloaded.clone()),
        )
        .await;

        assert!(
            result.is_err(),
            "expected the outer timeout to fire before the stream completed"
        );
        assert_eq!(
            downloaded.load(Ordering::Relaxed),
            5,
            "partial progress must survive cancellation of the streaming future"
        );
    }

    #[test]
    fn precache_store_key_uses_shared_prefix() {
        let key = precache_store_key(42);
        assert_eq!(key, "precached_torrent:42");
        assert!(key.starts_with(PRECACHE_STORE_PREFIX));
    }
}
