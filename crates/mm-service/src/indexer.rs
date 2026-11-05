//! Background indexing: vault file events → debounce → re-chunk → embed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mm_index::Priority;
use mm_index::debounce::Debouncer;
use mm_index::pipeline::{Embedder, embed_pending, refresh_note, scan_vault};
use mm_store::Store;
use notify::{RecursiveMode, Watcher};

use crate::ServiceError;

/// Model stamp of the current embedding setup; changing it re-embeds everything.
pub fn current_stamp() -> String {
    mm_index::model_stamp("embeddinggemma-2", mm_store::EMBEDDING_DIMS, 1)
}

/// Messages accepted by the indexer thread.
pub enum IndexerMsg {
    /// A file in the vault changed (from the watcher or the in-app editor).
    Touch(PathBuf, Priority),
    /// Embed pending chunks now (e.g. after a transcript sync or a model load).
    Kick,
    /// Switch to another vault (or none).
    SetVault(Option<PathBuf>),
    /// Use (or stop using) an embedder.
    SetEmbedder(Option<Arc<dyn Embedder + Send + Sync>>),
    /// While true, embedding yields the GPU to live captions.
    SetLive(bool),
    Stop,
}

/// Progress reported to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexerEvent {
    /// Notes or chunks changed; refresh counters.
    Updated { embedded: usize },
    Error(String),
}

/// Handle to the indexer thread.
#[derive(Clone)]
pub struct Indexer {
    tx: async_channel::Sender<IndexerMsg>,
}

impl Indexer {
    /// Starts the indexer thread.
    ///
    /// Params: `store` – shared store; `vault` – initial vault.
    /// Returns: the handle and an event stream for the UI.
    pub fn start(store: Arc<Mutex<Store>>, vault: Option<PathBuf>) -> (Self, async_channel::Receiver<IndexerEvent>) {
        let (tx, rx) = async_channel::unbounded();
        let (ev_tx, ev_rx) = async_channel::unbounded();
        let watcher_tx = tx.clone();
        std::thread::Builder::new()
            .name("indexer".into())
            .spawn(move || run(store, vault, rx, watcher_tx, ev_tx))
            .expect("spawn indexer thread");
        (Self { tx }, ev_rx)
    }

    /// Sends a message; a stopped indexer is logged, not fatal.
    pub fn send(&self, msg: IndexerMsg) {
        if self.tx.send_blocking(msg).is_err() {
            tracing::warn!("indexer thread is gone; message dropped");
        }
    }
}

/// Builds a recursive FSEvents watcher feeding `Touch` messages.
fn watch(vault: &Path, tx: async_channel::Sender<IndexerMsg>) -> Result<notify::RecommendedWatcher, ServiceError> {
    let root = vault.to_path_buf();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(ev) => {
            for p in ev.paths {
                let is_note = p.extension().is_some_and(|e| e == "md");
                // Only folders *inside* the vault count (.obsidian, .trash); the vault itself may live under a dot folder.
                let hidden = p
                    .strip_prefix(&root)
                    .map(|rel| rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')))
                    .unwrap_or(true);
                if is_note && !hidden {
                    let _ = tx.send_blocking(IndexerMsg::Touch(p, Priority::Interactive));
                }
            }
        }
        Err(e) => tracing::warn!("vault watcher: {e}"),
    })
    .map_err(|e| ServiceError::Index(format!("creating vault watcher: {e}")))?;
    w.watch(vault, RecursiveMode::Recursive)
        .map_err(|e| ServiceError::Index(format!("watching {}: {e}", vault.display())))?;
    Ok(w)
}

/// Indexer thread body.
fn run(
    store: Arc<Mutex<Store>>,
    mut vault: Option<PathBuf>,
    rx: async_channel::Receiver<IndexerMsg>,
    self_tx: async_channel::Sender<IndexerMsg>,
    events: async_channel::Sender<IndexerEvent>,
) {
    let report = |e: String| {
        tracing::warn!("indexer: {e}");
        let _ = events.send_blocking(IndexerEvent::Error(e));
    };
    let mut debouncer: Debouncer<PathBuf> = Debouncer::new(Duration::from_millis(1500), Duration::from_secs(10));
    let mut embedder: Option<Arc<dyn Embedder + Send + Sync>> = None;
    let mut live = false;
    let mut watcher = None;
    let mut need_scan = true;
    let stamp = current_stamp();

    loop {
        if need_scan {
            need_scan = false;
            watcher = None;
            if let Some(v) = &vault {
                match scan_vault(&mut store.lock().expect("store lock"), v) {
                    Ok(_) => {
                        let _ = events.send_blocking(IndexerEvent::Updated { embedded: 0 });
                    }
                    Err(e) => report(format!("scanning vault {}: {e}", v.display())),
                }
                match watch(v, self_tx.clone()) {
                    Ok(w) => watcher = Some(w),
                    Err(e) => report(e.to_string()),
                }
            }
        }
        let _keep_alive = &watcher;

        // Wait for the next message, or until a debounced file is due / embedding work remains.
        let timeout = debouncer
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(if embedder.is_some() && !live { 2 } else { 3600 }));
        let msg = match recv_timeout(&rx, timeout) {
            Ok(m) => Some(m),
            Err(true) => None,
            Err(false) => return, // channel closed
        };
        match msg {
            Some(IndexerMsg::Touch(p, _)) => debouncer.touch(p, Instant::now()),
            Some(IndexerMsg::Kick) | None => {}
            Some(IndexerMsg::SetVault(v)) => {
                vault = v;
                need_scan = true;
            }
            Some(IndexerMsg::SetEmbedder(e)) => embedder = e,
            Some(IndexerMsg::SetLive(l)) => live = l,
            Some(IndexerMsg::Stop) => return,
        }

        for path in debouncer.drain_due(Instant::now()) {
            if let Some(v) = &vault
                && let Err(e) = refresh_note(&mut store.lock().expect("store lock"), v, &path)
            {
                report(format!("indexing {}: {e}", path.display()));
            }
        }

        if let Some(emb) = &embedder {
            // During a meeting, embed only small text batches so captions keep the GPU.
            let batch = if live { 4 } else { 16 };
            match embed_pending(&mut store.lock().expect("store lock"), emb.as_ref(), &stamp, batch) {
                Ok(0) => {}
                Ok(n) => {
                    let _ = events.send_blocking(IndexerEvent::Updated { embedded: n });
                    let _ = self_tx.try_send(IndexerMsg::Kick);
                }
                Err(e) => report(e.to_string()),
            }
        }
    }
}

/// Receives with a timeout. `Err(true)` = timed out, `Err(false)` = closed.
fn recv_timeout<T>(rx: &async_channel::Receiver<T>, timeout: Duration) -> Result<T, bool> {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(m) => return Ok(m),
            Err(async_channel::TryRecvError::Closed) => return Err(false),
            Err(async_channel::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    return Err(true);
                }
                std::thread::sleep(Duration::from_millis(25).min(deadline - Instant::now()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OneHot;
    impl Embedder for OneHot {
        fn embed_documents(&self, items: &[mm_index::pipeline::EmbedItem]) -> Result<Vec<Vec<f32>>, String> {
            Ok(items.iter().map(|_| { let mut v = vec![0.0; mm_store::EMBEDDING_DIMS]; v[0] = 1.0; v }).collect())
        }
        fn modalities(&self) -> Vec<String> { vec!["text".into()] }
    }

    #[test]
    fn indexes_vault_changes_and_embeds_in_background() {
        let dir = tempfile::tempdir().unwrap();
        let vault = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(vault.join("a.md"), "# 预算评审\n\n内容").unwrap();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let (ix, events) = Indexer::start(store.clone(), Some(vault.clone()));
        ix.send(IndexerMsg::SetEmbedder(Some(Arc::new(OneHot))));

        // Edit the note; the watcher + debouncer must pick it up.
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(vault.join("b.md"), "# 发布计划\n\n上线").unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let s = store.lock().unwrap().index_stats(&current_stamp()).unwrap();
            if s.sources == 2 && s.embedded == s.chunks && s.chunks == 2 {
                break;
            }
            assert!(Instant::now() < deadline, "indexer did not converge: {s:?}");
            std::thread::sleep(Duration::from_millis(100));
        }
        ix.send(IndexerMsg::Stop);
        assert!(events.try_recv().is_ok());
    }
}
