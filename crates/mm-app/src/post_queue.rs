//! Queue of post-meeting jobs (refine → minutes).
//!
//! Stopping a recording only takes the short "stop" phase (capture/stop, save the live
//! transcript); the slow refine and minutes run here one meeting at a time. That frees the
//! recorder for the next meeting while earlier ones are still being processed, and keeps
//! refine passes from competing with each other for the GPU and the `agy` quota.
//!
//! The queue is plain data so its ordering rules can be tested without GPUI; `AppState`
//! drives it and runs the work on the background executor.

use mm_core::MeetingId;

/// Where a post-meeting job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostStage {
    /// capture/stop and the live transcript save are still running; the recorder is busy.
    Stopping,
    /// Stopped and waiting for earlier meetings to finish processing.
    Queued,
    /// Refine and minutes are running.
    Running,
    /// Finished (warnings may still be listed).
    Done,
    /// Failed before finishing; the recording is kept.
    Failed,
}

/// One meeting's post-processing, as the 会议中 page shows it.
#[derive(Debug, Clone)]
pub struct PostMeeting {
    pub meeting_id: MeetingId,
    pub title: String,
    pub stage: PostStage,
    /// Progress lines in the UI language, in order.
    pub steps: Vec<String>,
    /// Non-fatal problems (and the error, when failed).
    pub warnings: Vec<String>,
}

impl PostMeeting {
    /// Whether the job still has work left.
    ///
    /// Returns: `true` while stopping, queued or running.
    pub fn active(&self) -> bool {
        matches!(
            self.stage,
            PostStage::Stopping | PostStage::Queued | PostStage::Running
        )
    }
}

/// Post-meeting jobs in arrival order. `P` is what the processing phase needs (the stopped
/// meeting and its files); it is held only while the job is queued.
#[derive(Debug)]
pub struct PostQueue<P> {
    jobs: Vec<(PostMeeting, Option<P>)>,
}

impl<P> Default for PostQueue<P> {
    /// An empty queue.
    fn default() -> Self {
        Self { jobs: Vec::new() }
    }
}

impl<P> PostQueue<P> {
    /// Adds a meeting whose recording is being stopped.
    ///
    /// Params: `id`; `title` – shown on the card.
    pub fn push_stopping(&mut self, id: MeetingId, title: String) {
        // A meeting recorded again replaces its old (finished) card.
        self.jobs.retain(|(j, _)| j.meeting_id != id || j.active());
        self.jobs.push((
            PostMeeting {
                meeting_id: id,
                title,
                stage: PostStage::Stopping,
                steps: Vec::new(),
                warnings: Vec::new(),
            },
            None,
        ));
    }

    /// Records that the stop phase finished and queues the processing phase.
    ///
    /// Params: `id`; `payload` – input of the processing phase; `steps`, `warnings` – from
    /// the stop phase.
    pub fn stopped(
        &mut self,
        id: MeetingId,
        payload: P,
        steps: Vec<String>,
        warnings: Vec<String>,
    ) {
        if let Some((job, slot)) = self.find_mut(id, PostStage::Stopping) {
            job.stage = PostStage::Queued;
            job.steps.extend(steps);
            job.warnings.extend(warnings);
            *slot = Some(payload);
        }
    }

    /// Takes the next queued job when nothing is running, and marks it running.
    ///
    /// Returns: the meeting id and its payload, or `None` when busy or empty.
    pub fn start_next(&mut self) -> Option<(MeetingId, P)> {
        if self.jobs.iter().any(|(j, _)| j.stage == PostStage::Running) {
            return None;
        }
        let (job, slot) = self
            .jobs
            .iter_mut()
            .find(|(j, _)| j.stage == PostStage::Queued)?;
        job.stage = PostStage::Running;
        Some((job.meeting_id, slot.take()?))
    }

    /// Appends a progress line to an active job.
    ///
    /// Params: `id`; `step` – in the UI language.
    pub fn step(&mut self, id: MeetingId, step: String) {
        if let Some((job, _)) = self
            .jobs
            .iter_mut()
            .find(|(j, _)| j.meeting_id == id && j.active())
        {
            job.steps.push(step);
        }
    }

    /// Marks an active job finished.
    ///
    /// Params: `id`; `warnings` – non-fatal problems to list.
    pub fn finish(&mut self, id: MeetingId, warnings: Vec<String>) {
        self.close(id, PostStage::Done, warnings);
    }

    /// Marks an active job failed.
    ///
    /// Params: `id`; `error` – listed with the warnings.
    pub fn fail(&mut self, id: MeetingId, error: String) {
        self.close(id, PostStage::Failed, vec![error]);
    }

    /// Drops finished and failed cards (the old single card was cleared on the next start).
    pub fn clear_finished(&mut self) {
        self.jobs.retain(|(j, _)| j.active());
    }

    /// Whether a recording is still being stopped (the recorder cannot start yet).
    ///
    /// Returns: `true` while any job is in [`PostStage::Stopping`].
    pub fn stopping(&self) -> bool {
        self.jobs
            .iter()
            .any(|(j, _)| j.stage == PostStage::Stopping)
    }

    /// Number of jobs still running or waiting.
    ///
    /// Returns: (running, waiting), where waiting counts stopping and queued jobs.
    pub fn counts(&self) -> (usize, usize) {
        let running = self
            .jobs
            .iter()
            .filter(|(j, _)| j.stage == PostStage::Running)
            .count();
        let waiting = self
            .jobs
            .iter()
            .filter(|(j, _)| matches!(j.stage, PostStage::Stopping | PostStage::Queued))
            .count();
        (running, waiting)
    }

    /// Position of a queued job among those waiting (1 = next).
    ///
    /// Params: `id`.
    /// Returns: the position, or `None` when the job is not queued.
    pub fn position(&self, id: MeetingId) -> Option<usize> {
        self.jobs
            .iter()
            .filter(|(j, _)| j.stage == PostStage::Queued)
            .position(|(j, _)| j.meeting_id == id)
            .map(|i| i + 1)
    }

    /// Cards in arrival order.
    ///
    /// Returns: clones for rendering.
    pub fn cards(&self) -> Vec<PostMeeting> {
        self.jobs.iter().map(|(j, _)| j.clone()).collect()
    }

    /// Finds a job in a given stage.
    ///
    /// Params: `id`; `stage`.
    /// Returns: the job and its payload slot.
    fn find_mut(
        &mut self,
        id: MeetingId,
        stage: PostStage,
    ) -> Option<(&mut PostMeeting, &mut Option<P>)> {
        self.jobs
            .iter_mut()
            .find(|(j, _)| j.meeting_id == id && j.stage == stage)
            .map(|(j, p)| (j, p))
    }

    /// Ends an active job.
    ///
    /// Params: `id`; `stage` – Done or Failed; `warnings` – appended.
    fn close(&mut self, id: MeetingId, stage: PostStage, warnings: Vec<String>) {
        if let Some((job, slot)) = self
            .jobs
            .iter_mut()
            .find(|(j, _)| j.meeting_id == id && j.active())
        {
            job.stage = stage;
            job.warnings.extend(warnings);
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stable meeting id per small number, so tests read like "meeting 1, meeting 2".
    fn id(n: u128) -> MeetingId {
        MeetingId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn jobs_run_one_at_a_time_in_stop_order() {
        let mut q = PostQueue::<&str>::default();
        q.push_stopping(id(1), "A".into());
        assert!(q.stopping());
        assert_eq!(q.start_next(), None, "nothing to run while still stopping");
        q.stopped(id(1), "a", vec![], vec![]);
        assert!(!q.stopping());
        q.push_stopping(id(2), "B".into());
        q.stopped(id(2), "b", vec![], vec![]);

        assert_eq!(q.start_next(), Some((id(1), "a")));
        assert_eq!(q.start_next(), None, "B waits while A runs");
        assert_eq!(q.position(id(2)), Some(1));
        assert_eq!(q.counts(), (1, 1));

        q.finish(id(1), vec!["w".into()]);
        assert_eq!(q.start_next(), Some((id(2), "b")));
        q.fail(id(2), "boom".into());
        assert_eq!(q.counts(), (0, 0));
        let cards = q.cards();
        assert_eq!(cards[0].stage, PostStage::Done);
        assert_eq!(cards[0].warnings, vec!["w".to_string()]);
        assert_eq!(cards[1].stage, PostStage::Failed);
    }

    #[test]
    fn a_slow_stop_does_not_hold_back_later_meetings() {
        let mut q = PostQueue::<&str>::default();
        q.push_stopping(id(1), "A".into());
        q.push_stopping(id(2), "B".into());
        q.stopped(id(2), "b", vec!["stopped".into()], vec![]);
        assert_eq!(q.start_next(), Some((id(2), "b")));
        assert_eq!(q.cards()[1].steps, vec!["stopped".to_string()]);
    }

    #[test]
    fn steps_go_to_the_active_job_and_clear_keeps_active_ones() {
        let mut q = PostQueue::<()>::default();
        q.push_stopping(id(1), "A".into());
        q.stopped(id(1), (), vec![], vec![]);
        q.start_next();
        q.step(id(1), "refining".into());
        q.finish(id(1), vec![]);
        q.step(id(1), "late".into());
        q.push_stopping(id(2), "B".into());
        assert_eq!(q.cards()[0].steps, vec!["refining".to_string()]);

        q.clear_finished();
        let cards = q.cards();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].meeting_id, id(2));
    }

    #[test]
    fn recording_a_meeting_again_replaces_its_finished_card() {
        let mut q = PostQueue::<()>::default();
        q.push_stopping(id(1), "A".into());
        q.fail(id(1), "x".into());
        q.push_stopping(id(1), "A".into());
        let cards = q.cards();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].stage, PostStage::Stopping);
    }
}
