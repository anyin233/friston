//! Ordered background writes for the live session.
//!
//! Captions arrive many times a second; writing each one from the UI thread would stall
//! rendering whenever the indexer holds the store lock. A single writer thread keeps the
//! writes in arrival order (a relabel must land after the segments it renames).

use std::sync::{Arc, Mutex};

use mm_core::{Meeting, MeetingId, SpeakerLabel, TranscriptSegment};
use mm_i18n::trf;
use mm_service::AppConfig;
use mm_service::indexer::{Indexer, IndexerMsg};
use mm_store::{AsrUsageRecord, SegmentPass, Store};

/// Work for the writer thread.
pub enum WriteMsg {
    /// Persist a final caption (live pass) with its engine utterance id.
    Segment(TranscriptSegment, u64),
    /// Persist one cloud live-ASR usage delta.
    AsrUsage(AsrUsageRecord),
    /// Apply an engine `speaker/relabel` to stored segments.
    Relabel(MeetingId, SpeakerLabel, SpeakerLabel),
    /// Rewrite the note's transcript block and re-index the transcript.
    Sync(Box<AppConfig>, Box<Meeting>),
    /// Signals the sender once everything queued before it is written.
    Flush(async_channel::Sender<()>),
}

/// Handle to the writer thread. Cheap to clone.
#[derive(Clone)]
pub struct LiveWriter {
    tx: async_channel::Sender<WriteMsg>,
}

impl LiveWriter {
    /// Starts the writer thread.
    ///
    /// Params: `store` – shared store; `indexer` – kicked after syncs so new chunks embed.
    /// Returns: the handle and a stream of error messages for the UI.
    pub fn start(
        store: Arc<Mutex<Store>>,
        indexer: Indexer,
    ) -> (Self, async_channel::Receiver<String>) {
        let (tx, rx) = async_channel::unbounded::<WriteMsg>();
        let (err_tx, err_rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("live-writer".into())
            .spawn(move || {
                while let Ok(msg) = rx.recv_blocking() {
                    if let Err(e) = handle(&store, &indexer, msg) {
                        tracing::warn!("live writer: {e}");
                        let _ = err_tx.send_blocking(e);
                    }
                }
            })
            .expect("spawn live writer thread");
        (Self { tx }, err_rx)
    }

    /// Queues a write; a stopped writer is logged, not fatal.
    ///
    /// Params: `msg` – the work.
    pub fn send(&self, msg: WriteMsg) {
        if self.tx.send_blocking(msg).is_err() {
            tracing::warn!("live writer thread is gone; write dropped");
        }
    }

    /// Blocks until every queued write is done. Call from a background thread only.
    pub fn flush_blocking(&self) {
        let (tx, rx) = async_channel::bounded(1);
        self.send(WriteMsg::Flush(tx));
        let _ = rx.recv_blocking();
    }
}

/// Executes one message.
///
/// Params: `store`; `indexer`; `msg`.
/// Returns: an error message with context on failure.
fn handle(store: &Arc<Mutex<Store>>, indexer: &Indexer, msg: WriteMsg) -> Result<(), String> {
    match msg {
        WriteMsg::Segment(seg, utterance) => lock(store)?
            .add_segment(&seg, SegmentPass::Live, Some(utterance))
            .map(|_| ())
            .map_err(|e| {
                trf!(
                    "保存字幕（第 {utterance} 句）失败：{e}",
                    "Couldn’t save caption (utterance {utterance}): {e}",
                )
            }),
        WriteMsg::AsrUsage(rec) => lock(store)?.record_asr_usage(&rec).map_err(|e| {
            trf!(
                "记录云端转写用量失败：{e}",
                "Couldn’t record cloud ASR usage: {e}"
            )
        }),
        WriteMsg::Relabel(meeting, from, to) => lock(store)?
            .relabel_segments(meeting, &from, &to)
            .map(|_| ())
            .map_err(|e| trf!("更新说话人失败：{e}", "Couldn’t update speaker: {e}")),
        WriteMsg::Sync(cfg, meeting) => {
            mm_service::flow::sync_transcript(&cfg, &mut *lock(store)?, &meeting).map_err(|e| {
                trf!(
                    "同步转写到笔记失败（{}）：{e}",
                    "Couldn’t sync transcript to note ({}): {e}",
                    meeting.title,
                )
            })?;
            indexer.send(IndexerMsg::Kick);
            Ok(())
        }
        WriteMsg::Flush(done) => {
            let _ = done.send_blocking(());
            Ok(())
        }
    }
}

/// Locks the store, turning a poisoned lock into an error message.
///
/// Params: `store`.
/// Returns: the guard.
pub fn lock(store: &Arc<Mutex<Store>>) -> Result<std::sync::MutexGuard<'_, Store>, String> {
    store
        .lock()
        .map_err(|e| trf!("数据库锁异常：{e}", "Database lock poisoned: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_core::AudioOrigin;

    #[test]
    fn writes_are_ordered_and_flush_waits() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let (indexer, _ev) = Indexer::start(store.clone(), None);
        let (w, errors) = LiveWriter::start(store.clone(), indexer.clone());
        let m = mm_store::new_meeting("t", chrono::Utc::now());
        store.lock().unwrap().upsert_meeting(&m).unwrap();
        let seg = TranscriptSegment {
            meeting_id: m.id,
            start_ms: 0,
            end_ms: 10,
            text: "好".into(),
            language: None,
            speaker: SpeakerLabel::Cluster { index: 2 },
            origin: AudioOrigin::Microphone,
            spatial: None,
        };
        w.send(WriteMsg::Segment(seg, 1));
        w.send(WriteMsg::Relabel(
            m.id,
            SpeakerLabel::Cluster { index: 2 },
            SpeakerLabel::Cluster { index: 1 },
        ));
        w.flush_blocking();
        let t = store.lock().unwrap().transcript(m.id).unwrap();
        assert_eq!(t[0].speaker, SpeakerLabel::Cluster { index: 1 });
        assert!(errors.try_recv().is_err());
        indexer.send(IndexerMsg::Stop);
    }
}
