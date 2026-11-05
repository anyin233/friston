//! Live caption state and speaker display names/hues.
//!
//! Both are pure state machines: the UI feeds them engine notices and renders
//! what they hold, which keeps them testable without GPUI or an engine.

use std::collections::HashMap;

use mm_core::{
    AudioOrigin, CaptionFinal, CaptionPartial, MeetingId, PersonId, SpeakerLabel,
    SpeakerRelabel, TranscriptSegment,
};

/// One line on the caption board.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptionLine {
    pub utterance_id: u64,
    pub origin: AudioOrigin,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub text: String,
    pub speaker: SpeakerLabel,
    /// `false` while the engine may still revise the text.
    pub is_final: bool,
}

/// Caption lines of the running session, in utterance order.
#[derive(Debug, Default, Clone)]
pub struct CaptionBoard {
    session_id: Option<String>,
    lines: Vec<CaptionLine>,
}

impl CaptionBoard {
    /// Starts a fresh board for a capture session; notices of other sessions are ignored.
    ///
    /// Params: `session_id` – from `capture/start`.
    pub fn begin(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
        self.lines.clear();
    }

    /// Whether a notice belongs to the current session.
    fn ours(&self, session: &str) -> bool {
        self.session_id.as_deref() == Some(session)
    }

    /// Index of the line for `utterance_id`, searching from the end (new lines are hot).
    fn find(&self, id: u64) -> Option<usize> {
        self.lines.iter().rposition(|l| l.utterance_id == id)
    }

    /// Applies a volatile hypothesis.
    ///
    /// Params: `p` – the notice.
    /// Returns: `true` if the board changed.
    pub fn apply_partial(&mut self, p: &CaptionPartial) -> bool {
        if !self.ours(&p.session_id) {
            return false;
        }
        match self.find(p.utterance_id) {
            // A late partial must not overwrite text the engine already finalised.
            Some(i) if self.lines[i].is_final => false,
            Some(i) => {
                let l = &mut self.lines[i];
                l.text = p.text.clone();
                l.speaker = p.speaker.clone();
                true
            }
            None => {
                self.lines.push(CaptionLine {
                    utterance_id: p.utterance_id,
                    origin: p.origin,
                    start_ms: p.start_ms,
                    end_ms: None,
                    text: p.text.clone(),
                    speaker: p.speaker.clone(),
                    is_final: false,
                });
                true
            }
        }
    }

    /// Applies stable text.
    ///
    /// Params: `f` – the notice; `meeting` – owner of the resulting segment.
    /// Returns: the segment to persist, or `None` for a foreign session or empty text.
    pub fn apply_final(&mut self, f: &CaptionFinal, meeting: MeetingId) -> Option<TranscriptSegment> {
        if !self.ours(&f.session_id) {
            return None;
        }
        let line = CaptionLine {
            utterance_id: f.utterance_id,
            origin: f.origin,
            start_ms: f.start_ms,
            end_ms: Some(f.end_ms),
            text: f.text.clone(),
            speaker: f.speaker.clone(),
            is_final: true,
        };
        match self.find(f.utterance_id) {
            Some(i) if f.text.trim().is_empty() => {
                // The engine retracted a false start (noise detected as speech).
                self.lines.remove(i);
            }
            Some(i) => self.lines[i] = line,
            None if !f.text.trim().is_empty() => self.lines.push(line),
            None => {}
        }
        if f.text.trim().is_empty() {
            return None;
        }
        Some(TranscriptSegment {
            meeting_id: meeting,
            start_ms: f.start_ms,
            end_ms: f.end_ms,
            text: f.text.trim().to_string(),
            language: f.language.clone(),
            speaker: f.speaker.clone(),
            origin: f.origin,
            spatial: f.spatial,
        })
    }

    /// Applies a retroactive relabel to every line.
    ///
    /// Params: `r` – the notice.
    /// Returns: number of lines changed.
    pub fn apply_relabel(&mut self, r: &SpeakerRelabel) -> usize {
        if !self.ours(&r.session_id) {
            return 0;
        }
        let mut n = 0;
        for l in &mut self.lines {
            if l.speaker == r.from {
                l.speaker = r.to.clone();
                n += 1;
            }
        }
        n
    }

    /// All lines.
    pub fn lines(&self) -> &[CaptionLine] {
        &self.lines
    }

    /// The last `n` lines (floating caption window).
    pub fn tail(&self, n: usize) -> &[CaptionLine] {
        &self.lines[self.lines.len().saturating_sub(n)..]
    }
}

/// Hues assigned to anonymous clusters in order; chosen to stay apart on a dark background.
const CLUSTER_HUES: [u16; 8] = [210, 28, 150, 330, 265, 95, 185, 0];

/// Stable hue for a person who has none stored yet (identity: one person, one hue).
///
/// Params: `id` – person.
/// Returns: hue in degrees.
pub fn person_hue(id: PersonId) -> u16 {
    let h = blake3::hash(id.0.as_bytes());
    u16::from_le_bytes([h.as_bytes()[0], h.as_bytes()[1]]) % 360
}

/// Resolves speaker labels to display names and hues for one meeting.
#[derive(Debug, Default, Clone)]
pub struct SpeakerDirectory {
    people: HashMap<PersonId, (String, u16)>,
    /// Cluster index → person, as named by the user in this meeting.
    named_clusters: HashMap<u32, PersonId>,
}

impl SpeakerDirectory {
    /// Builds the directory.
    ///
    /// Params: `people` – `(person, name, stored hue)`; `named_clusters` – user assignments.
    /// Returns: the directory.
    pub fn new(people: impl IntoIterator<Item = (PersonId, String, Option<u16>)>, named_clusters: impl IntoIterator<Item = (u32, PersonId)>) -> Self {
        Self {
            people: people
                .into_iter()
                .map(|(id, name, hue)| (id, (name, hue.unwrap_or_else(|| person_hue(id)))))
                .collect(),
            named_clusters: named_clusters.into_iter().collect(),
        }
    }

    /// Records that cluster `index` is `person` (the user named "说话人 N").
    pub fn name_cluster(&mut self, index: u32, person: PersonId) {
        self.named_clusters.insert(index, person);
    }

    /// The person behind a label, if known.
    pub fn person(&self, label: &SpeakerLabel) -> Option<PersonId> {
        match label {
            SpeakerLabel::Known { person_id, .. } => Some(*person_id),
            SpeakerLabel::Cluster { index } => self.named_clusters.get(index).copied(),
            SpeakerLabel::Unknown => None,
        }
    }

    /// Display name: the person's name, else "说话人 N", else "未识别".
    ///
    /// Params: `label` – speaker label.
    /// Returns: the name.
    pub fn name(&self, label: &SpeakerLabel) -> String {
        if let Some((name, _)) = self.person(label).and_then(|p| self.people.get(&p)) {
            return name.clone();
        }
        match label {
            SpeakerLabel::Cluster { index } => format!("说话人 {index}"),
            _ => "未识别".into(),
        }
    }

    /// Hue for a label; the person's hue wins so naming a cluster recolours it everywhere.
    ///
    /// Params: `label` – speaker label.
    /// Returns: hue in degrees, or `None` for unknown speakers (rendered neutral).
    pub fn hue(&self, label: &SpeakerLabel) -> Option<u16> {
        if let Some(p) = self.person(label) {
            return Some(self.people.get(&p).map(|(_, h)| *h).unwrap_or_else(|| person_hue(p)));
        }
        match label {
            SpeakerLabel::Cluster { index } => Some(CLUSTER_HUES[*index as usize % CLUSTER_HUES.len()]),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partial(id: u64, text: &str) -> CaptionPartial {
        CaptionPartial {
            session_id: "s".into(),
            utterance_id: id,
            origin: AudioOrigin::Microphone,
            start_ms: id * 1000,
            text: text.into(),
            speaker: SpeakerLabel::Cluster { index: 1 },
        }
    }

    fn fin(id: u64, text: &str) -> CaptionFinal {
        CaptionFinal {
            session_id: "s".into(),
            utterance_id: id,
            origin: AudioOrigin::Microphone,
            start_ms: id * 1000,
            end_ms: id * 1000 + 800,
            text: text.into(),
            language: Some("zh".into()),
            speaker: SpeakerLabel::Cluster { index: 2 },
            spatial: None,
        }
    }

    #[test]
    fn partials_are_replaced_and_finalised() {
        let mut b = CaptionBoard::default();
        b.begin("s");
        let m = MeetingId::new();
        assert!(b.apply_partial(&partial(1, "大家")));
        assert!(b.apply_partial(&partial(1, "大家好")));
        assert!(b.apply_partial(&partial(2, "开")));
        assert_eq!(b.lines().len(), 2);
        let seg = b.apply_final(&fin(1, " 大家好。 "), m).unwrap();
        assert_eq!(seg.text, "大家好。");
        assert!(b.lines()[0].is_final);
        assert!(!b.apply_partial(&partial(1, "迟到的")), "late partial ignored");
        // Retraction removes the line and yields nothing to persist.
        assert!(b.apply_final(&fin(2, ""), m).is_none());
        assert_eq!(b.lines().len(), 1);
        // Foreign session.
        let mut other = partial(3, "x");
        other.session_id = "old".into();
        assert!(!b.apply_partial(&other));
    }

    #[test]
    fn relabel_and_tail() {
        let mut b = CaptionBoard::default();
        b.begin("s");
        let m = MeetingId::new();
        b.apply_final(&fin(1, "一"), m);
        b.apply_final(&fin(2, "二"), m);
        let n = b.apply_relabel(&SpeakerRelabel {
            session_id: "s".into(),
            from: SpeakerLabel::Cluster { index: 2 },
            to: SpeakerLabel::Cluster { index: 1 },
        });
        assert_eq!(n, 2);
        assert_eq!(b.tail(1)[0].text, "二");
        assert_eq!(b.tail(10).len(), 2);
    }

    #[test]
    fn directory_names_and_hues_follow_the_person() {
        let zhao = PersonId::new();
        let mut d = SpeakerDirectory::new([(zhao, "赵磊".to_string(), Some(42))], []);
        let c3 = SpeakerLabel::Cluster { index: 3 };
        assert_eq!(d.name(&c3), "说话人 3");
        assert_eq!(d.hue(&c3), Some(CLUSTER_HUES[3]));
        d.name_cluster(3, zhao);
        assert_eq!(d.name(&c3), "赵磊");
        assert_eq!(d.hue(&c3), Some(42));
        let known = SpeakerLabel::Known { person_id: zhao, score: 0.8 };
        assert_eq!((d.name(&known), d.hue(&known)), ("赵磊".to_string(), Some(42)));
        assert_eq!(d.name(&SpeakerLabel::Unknown), "未识别");
        let stranger = PersonId::new();
        assert_eq!(person_hue(stranger), person_hue(stranger), "derived hue is stable");
    }
}
