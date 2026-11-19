//! Post-meeting transcription helpers for external speech/agent models (Codex, OpenAI, Gemini).
//!
//! Provides macOS `afconvert` chunking + silence-gated utterance segmentation for cloud audio
//! endpoints, maps external sentences back onto this meeting's speakers, and provides WAV I/O
//! helpers for synthesis and integration tests.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;

use mm_core::{AudioOrigin, MeetingId, SpeakerLabel, TranscriptSegment};
use mm_llm::HeardLine;

use crate::ServiceError;

/// Longest audio part sent in one track-level run (safely below Gemini's 30-min diarization cap
/// and OpenAI's 25 MB upload limit at 16 kHz mono AAC).
pub const CHUNK_MS: u64 = 15 * 60 * 1000;

/// Maximum speech span when splitting audio for OpenAI models that omit segment timestamps.
pub const DEFAULT_UTTERANCE_MS: u64 = 45 * 1000;

/// Tracks quieter than this (RMS, dBFS) are treated as silent and dropped.
pub const SILENCE_DBFS: f64 = -70.0;

/// Sample rate of prepared audio chunks.
const RATE: u32 = 16_000;

/// One prepared audio chunk on disk ready for cloud transcription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedChunk {
    /// Start offset relative to the beginning of the recording, in milliseconds.
    pub offset_ms: u64,
    /// Chunk duration, in milliseconds.
    pub duration_ms: u64,
    /// Recorded stream the chunk came from.
    pub origin: AudioOrigin,
    /// Absolute path to the `.m4a` chunk file inside the work directory.
    pub path: PathBuf,
}

/// File stem used for a track.
fn stem(origin: AudioOrigin) -> &'static str {
    match origin {
        AudioOrigin::Microphone => "microphone",
        AudioOrigin::System => "system",
    }
}

/// Runs macOS `/usr/bin/afconvert`.
fn afconvert(args: &[&str]) -> Result<(), ServiceError> {
    let out = Command::new("/usr/bin/afconvert")
        .args(args)
        .output()
        .map_err(|e| ServiceError::External(format!("starting afconvert: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::External(format!(
            "afconvert {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// RMS level in dBFS.
///
/// Params: `samples` – 16-bit PCM.
/// Returns: the level; −∞ maps to −180 for empty or all-zero input.
pub fn dbfs(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return -180.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum / samples.len() as f64).sqrt() / 32_768.0;
    if rms <= 0.0 {
        -180.0
    } else {
        20.0 * rms.log10()
    }
}

/// Splits `len` samples into consecutive ranges of at most `per_chunk`.
///
/// Params: `len`; `per_chunk` – clamped to at least 1.
/// Returns: the ranges (empty for `len == 0`).
pub fn split_ranges(len: usize, per_chunk: usize) -> Vec<Range<usize>> {
    (0..len)
        .step_by(per_chunk.max(1))
        .map(|start| start..(start + per_chunk.max(1)).min(len))
        .collect()
}

/// Segments 16-bit PCM mono audio into active speech spans bounded by `max_span_ms`.
///
/// Uses 50 ms frame RMS energy with an adaptive noise-floor threshold, merges brief pauses
/// (< 600 ms), adds 150 ms edge padding, drops short blips (< 250 ms), and splits long
/// monologues at the quietest 100 ms window inside their final 10 seconds.
///
/// Params: `samples` – PCM samples; `rate` – sample rate (Hz); `max_span_ms` – max chunk length.
/// Returns: non-overlapping active speech ranges in ascending sample order.
pub fn find_speech_spans(samples: &[i16], rate: u32, max_span_ms: u64) -> Vec<Range<usize>> {
    if samples.is_empty() || rate == 0 || dbfs(samples) < SILENCE_DBFS {
        return Vec::new();
    }
    let frame = ((rate as usize * 50) / 1000).max(1);
    let mut frame_dbs: Vec<f64> = samples.chunks(frame).map(dbfs).collect();
    if frame_dbs.is_empty() {
        return Vec::new();
    }
    let mut sorted = frame_dbs.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let p10 = sorted[(sorted.len() / 10).min(sorted.len() - 1)];
    let threshold = (p10 + 10.0).clamp(-52.0, -35.0);

    // Collect raw active spans.
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut active_start: Option<usize> = None;
    for (i, db) in frame_dbs.drain(..).enumerate() {
        let s = i * frame;
        let e = ((i + 1) * frame).min(samples.len());
        if db >= threshold {
            active_start.get_or_insert(s);
        } else if let Some(start) = active_start.take() {
            spans.push(start..e.saturating_sub(frame).max(start));
        }
    }
    if let Some(start) = active_start {
        spans.push(start..samples.len());
    }
    if spans.is_empty() {
        return Vec::new();
    }

    // Merge pauses shorter than 600 ms, then pad by 150 ms and drop short blips (< 250 ms).
    let merge_gap = (rate as usize * 600) / 1000;
    let pad = (rate as usize * 150) / 1000;
    let min_speech = (rate as usize * 250) / 1000;

    let mut merged: Vec<Range<usize>> = Vec::new();
    for span in spans {
        if let Some(prev) = merged.last_mut()
            && span.start <= prev.end + merge_gap
        {
            prev.end = prev.end.max(span.end);
        } else {
            merged.push(span);
        }
    }

    let mut padded: Vec<Range<usize>> = Vec::new();
    for span in merged {
        if span.len() < min_speech {
            continue;
        }
        let s = span.start.saturating_sub(pad);
        let e = (span.end + pad).min(samples.len());
        if let Some(prev) = padded.last_mut()
            && s <= prev.end
        {
            prev.end = prev.end.max(e);
        } else {
            padded.push(s..e);
        }
    }

    // Bound each span to `max_span_ms`, splitting near the quietest 100 ms window in its last 10 s.
    let max_samples = ((rate as u64 * max_span_ms.max(1000)) / 1000) as usize;
    let search_tail = (rate as usize) * 10;
    let win = ((rate as usize * 100) / 1000).max(1);
    let mut bounded = Vec::new();
    for span in padded {
        let mut cur = span.start;
        while span.end - cur > max_samples {
            let hard_end = cur + max_samples;
            let search_start = hard_end
                .saturating_sub(search_tail)
                .max(cur + rate as usize);
            let mut best_pos = hard_end;
            if search_start + win < hard_end {
                let mut best_db = f64::INFINITY;
                let mut pos = search_start;
                while pos + win <= hard_end {
                    let db = dbfs(&samples[pos..pos + win]);
                    if db < best_db {
                        best_db = db;
                        best_pos = pos + win / 2;
                    }
                    pos += win;
                }
            }
            let cut = best_pos.clamp(cur + 1, span.end);
            bounded.push(cur..cut);
            cur = cut;
        }
        if span.end > cur {
            bounded.push(cur..span.end);
        }
    }
    bounded
}

/// Decodes each recording to 16 kHz mono PCM and writes `.m4a` chunks for the supplied ranges.
fn prepare_with_ranges(
    files: &[(AudioOrigin, PathBuf)],
    work: &Path,
    plan_ranges: impl Fn(&[(AudioOrigin, Vec<i16>)]) -> Vec<(AudioOrigin, Range<usize>)>,
) -> Result<Vec<PreparedChunk>, ServiceError> {
    match std::fs::remove_dir_all(work) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ServiceError::External(format!(
                "clearing {}: {e}",
                work.display()
            )));
        }
    }
    std::fs::create_dir_all(work)
        .map_err(|e| ServiceError::External(format!("creating {}: {e}", work.display())))?;

    let mut decoded = Vec::new();
    for (origin, src) in files {
        let wav = work.join(format!("{}-full.wav", stem(*origin)));
        afconvert(&[
            "-f",
            "WAVE",
            "-d",
            "LEI16@16000",
            "-c",
            "1",
            &src.to_string_lossy(),
            &wav.to_string_lossy(),
        ])?;
        let (rate, samples) = read_wav_mono16(&wav)?;
        if rate != RATE {
            return Err(ServiceError::External(format!(
                "{} decoded at {rate} Hz, expected {RATE}",
                wav.display()
            )));
        }
        std::fs::remove_file(&wav)
            .map_err(|e| ServiceError::External(format!("removing {}: {e}", wav.display())))?;
        decoded.push((*origin, samples));
    }

    let mut chunks = Vec::new();
    for (idx, (origin, range)) in plan_ranges(&decoded).into_iter().enumerate() {
        let Some((_, samples)) = decoded.iter().find(|(o, _)| *o == origin) else {
            continue;
        };
        let lo = range.start.min(samples.len());
        let hi = range.end.min(samples.len());
        if hi <= lo {
            continue;
        }
        let slice = &samples[lo..hi];
        if dbfs(slice) < SILENCE_DBFS {
            continue;
        }
        let wav = work.join(format!("{}-{idx:04}.wav", stem(origin)));
        let m4a = work.join(format!("{}-{idx:04}.m4a", stem(origin)));
        write_wav_mono16(&wav, RATE, slice)?;
        afconvert(&[
            "-f",
            "m4af",
            "-d",
            "aac",
            &wav.to_string_lossy(),
            &m4a.to_string_lossy(),
        ])?;
        std::fs::remove_file(&wav)
            .map_err(|e| ServiceError::External(format!("removing {}: {e}", wav.display())))?;
        chunks.push(PreparedChunk {
            offset_ms: lo as u64 * 1000 / RATE as u64,
            duration_ms: (hi - lo) as u64 * 1000 / RATE as u64,
            origin,
            path: m4a,
        });
    }
    chunks.sort_by_key(|c| c.offset_ms);
    Ok(chunks)
}

/// Converts recordings into silent-free `.m4a` chunks of at most `max_chunk_ms`.
///
/// Params: `files` – `(origin, recording)` pairs; `work` – scratch folder (cleared first);
/// `max_chunk_ms` – maximum chunk length in milliseconds.
/// Returns: non-silent chunks ordered by start time.
pub fn prepare_track_chunks(
    files: &[(AudioOrigin, PathBuf)],
    work: &Path,
    max_chunk_ms: u64,
) -> Result<Vec<PreparedChunk>, ServiceError> {
    let per_chunk = ((max_chunk_ms.max(1000) * RATE as u64) / 1000) as usize;
    prepare_with_ranges(files, work, |decoded| {
        let longest = decoded.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
        let mut out = Vec::new();
        for range in split_ranges(longest, per_chunk) {
            for (origin, _) in decoded {
                out.push((*origin, range.clone()));
            }
        }
        out
    })
}

/// Converts recordings into silence-gated speech-utterance `.m4a` chunks (at most `max_utterance_ms`).
///
/// Used for cloud endpoints (`gpt-transcribe`, `gpt-4o-transcribe`) that return flat text
/// without segment timestamps so each utterance keeps accurate timestamps and speaker alignment.
///
/// Params: `files` – `(origin, recording)` pairs; `work` – scratch folder; `max_utterance_ms`.
/// Returns: active speech chunks ordered by start time.
pub fn prepare_utterance_chunks(
    files: &[(AudioOrigin, PathBuf)],
    work: &Path,
    max_utterance_ms: u64,
) -> Result<Vec<PreparedChunk>, ServiceError> {
    prepare_with_ranges(files, work, |decoded| {
        let mut out = Vec::new();
        for (origin, samples) in decoded {
            for range in find_speech_spans(samples, RATE, max_utterance_ms) {
                out.push((*origin, range));
            }
        }
        out
    })
}

/// Reads a 16-bit PCM mono WAV, skipping chunks such as afconvert's `FLLR` padding.
///
/// Params: `path`.
/// Returns: `(sample rate, samples)`.
pub fn read_wav_mono16(path: &Path) -> Result<(u32, Vec<i16>), ServiceError> {
    let bad = |why: &str| ServiceError::External(format!("{}: {why}", path.display()));
    let bytes = std::fs::read(path)
        .map_err(|e| ServiceError::External(format!("reading {}: {e}", path.display())))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let mut pos = 12;
    let mut rate = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size =
            u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().expect("4 bytes")) as usize;
        let body = pos + 8;
        let end = body.saturating_add(size).min(bytes.len());
        match id {
            b"fmt " => {
                if end - body < 16 {
                    return Err(bad("short fmt chunk"));
                }
                let f = &bytes[body..end];
                let format = u16::from_le_bytes([f[0], f[1]]);
                let channels = u16::from_le_bytes([f[2], f[3]]);
                let bits = u16::from_le_bytes([f[14], f[15]]);
                if format != 1 || channels != 1 || bits != 16 {
                    return Err(bad(&format!(
                        "expected 16-bit PCM mono, got format {format}, {channels} ch, {bits} bit"
                    )));
                }
                rate = Some(u32::from_le_bytes([f[4], f[5], f[6], f[7]]));
            }
            b"data" => {
                let rate = rate.ok_or_else(|| bad("data chunk before fmt chunk"))?;
                let samples = bytes[body..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b))
                    .collect();
                return Ok((rate, samples));
            }
            _ => {}
        }
        // Chunks are padded to an even size.
        pos = body + size + (size & 1);
    }
    Err(bad("no data chunk"))
}

/// Writes a 16-bit PCM mono WAV.
///
/// Params: `path`; `rate`; `samples`.
/// Returns: `Ok(())`.
pub fn write_wav_mono16(path: &Path, rate: u32, samples: &[i16]) -> Result<(), ServiceError> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out)
        .map_err(|e| ServiceError::External(format!("writing {}: {e}", path.display())))
}

/// A model speaker label within one track: `(track, "说话人1")`.
type LabelKey<'a> = (AudioOrigin, &'a str);

/// Live speakers and how many milliseconds of a model label they overlap.
type Tally = Vec<(SpeakerLabel, u64)>;

/// Overlap of two time ranges in milliseconds.
fn overlap(a: (u64, u64), b: (u64, u64)) -> u64 {
    a.1.min(b.1).saturating_sub(a.0.max(b.0))
}

/// Next available anonymous cluster index for `origin`, staying above all live clusters
/// on that stream (remote system speakers start at `101`, matching `flow::merge_file_transcripts`).
fn next_cluster_start(live: &[TranscriptSegment], origin: AudioOrigin) -> u32 {
    let base = match origin {
        AudioOrigin::Microphone => 1,
        AudioOrigin::System => 101,
    };
    live.iter()
        .filter(|s| s.origin == origin)
        .filter_map(|s| match s.speaker {
            SpeakerLabel::Cluster { index } => Some(index),
            _ => None,
        })
        .max()
        .map_or(base, |m| m.saturating_add(1).max(base))
}

/// Turns an external run's sentences into final-pass segments with this meeting's speakers.
///
/// A sentence takes the speaker of the live segment covering most of it (at least half).
/// Otherwise it takes the live speaker that its model label overlaps most across the run.
/// Unmatched non-empty model labels are allocated distinct fresh `Cluster` indices per stream
/// (instead of collapsing together into `Unknown`); only empty labels map to `Unknown`.
///
/// Params: `meeting`; `offset_ms` – start offset added to each sentence; `heard` – the run's
/// sentences; `live` – the live-pass segments; `language` – caption language.
/// Returns: segments on the meeting's timeline.
pub fn heard_to_segments(
    meeting: MeetingId,
    offset_ms: u64,
    heard: &[HeardLine],
    live: &[TranscriptSegment],
    language: Option<String>,
) -> Vec<TranscriptSegment> {
    let span = |l: &HeardLine| (offset_ms + l.start_ms, offset_ms + l.end_ms.max(l.start_ms));
    // Votes per (track, model label): live speaker → overlapping milliseconds.
    let mut votes: Vec<(LabelKey, Tally)> = Vec::new();
    for l in heard {
        let key = (l.source, l.speaker.trim());
        let idx = match votes.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                votes.push((key, Vec::new()));
                votes.len() - 1
            }
        };
        for s in live.iter().filter(|s| s.origin == l.source) {
            let ms = overlap(span(l), (s.start_ms, s.end_ms));
            if ms == 0 {
                continue;
            }
            let tally = &mut votes[idx].1;
            match tally.iter_mut().find(|(who, _)| *who == s.speaker) {
                Some((_, total)) => *total += ms,
                None => tally.push((s.speaker.clone(), ms)),
            }
        }
    }
    let by_label = |key: (AudioOrigin, &str)| {
        votes
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, tally)| tally.iter().max_by_key(|(_, ms)| *ms))
            .map(|(who, _)| who.clone())
    };

    let mut next_mic = next_cluster_start(live, AudioOrigin::Microphone);
    let mut next_sys = next_cluster_start(live, AudioOrigin::System);
    let mut fresh: Vec<(LabelKey<'_>, SpeakerLabel)> = Vec::new();
    let mut out = Vec::new();

    for l in heard {
        let text = l.text.trim();
        if text.is_empty() {
            continue;
        }
        let (start, end) = span(l);
        let label = l.speaker.trim();
        let direct = live
            .iter()
            .filter(|s| s.origin == l.source)
            .map(|s| (overlap((start, end), (s.start_ms, s.end_ms)), s))
            .filter(|(ms, _)| *ms > 0 && ms * 2 >= (end - start))
            .max_by_key(|(ms, _)| *ms)
            .map(|(_, s)| s.speaker.clone());
        let speaker = if let Some(s) = direct.or_else(|| by_label((l.source, label))) {
            s
        } else if label.is_empty() {
            SpeakerLabel::Unknown
        } else if let Some((_, existing)) = fresh.iter().find(|(k, _)| *k == (l.source, label)) {
            existing.clone()
        } else {
            let next = match l.source {
                AudioOrigin::Microphone => &mut next_mic,
                AudioOrigin::System => &mut next_sys,
            };
            let assigned = SpeakerLabel::Cluster { index: *next };
            *next = next.saturating_add(1);
            fresh.push(((l.source, label), assigned.clone()));
            assigned
        };
        out.push(TranscriptSegment {
            meeting_id: meeting,
            start_ms: start,
            end_ms: end,
            text: text.to_string(),
            language: language.clone(),
            speaker,
            origin: l.source,
            spatial: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(origin: AudioOrigin, start: u64, end: u64, cluster: u32) -> TranscriptSegment {
        TranscriptSegment {
            meeting_id: MeetingId::new(),
            start_ms: start,
            end_ms: end,
            text: "x".into(),
            language: None,
            speaker: SpeakerLabel::Cluster { index: cluster },
            origin,
            spatial: None,
        }
    }

    fn heard(source: AudioOrigin, start: u64, end: u64, speaker: &str) -> HeardLine {
        HeardLine {
            source,
            start_ms: start,
            end_ms: end,
            speaker: speaker.into(),
            text: "好".into(),
        }
    }

    #[test]
    fn speakers_come_from_live_overlap_then_label_votes_then_fresh_clusters() {
        let mic = AudioOrigin::Microphone;
        let sys = AudioOrigin::System;
        let live_pass = vec![
            live(mic, 900_000, 905_000, 1),
            live(mic, 906_000, 910_000, 2),
            // Same time, other track: must never be borrowed.
            live(sys, 900_000, 990_000, 101),
        ];
        let lines = vec![
            heard(mic, 0, 5_000, "说话人1"),       // covered by cluster 1
            heard(mic, 6_000, 9_000, "说话人2"),   // covered by cluster 2
            heard(mic, 30_000, 31_000, "说话人1"), // no live segment: label vote → cluster 1
            heard(mic, 40_000, 41_000, "说话人3"), // unmatched mic label → fresh cluster 3
            heard(mic, 42_000, 43_000, "说话人3"), // same unmatched label → reuses cluster 3
            heard(sys, 100_000, 101_000, "远程1"), // unmatched system label → fresh cluster 102
            heard(mic, 45_000, 46_000, " "),       // empty speaker → Unknown
        ];
        let segs = heard_to_segments(
            MeetingId::new(),
            900_000,
            &lines,
            &live_pass,
            Some("zh".into()),
        );
        let who: Vec<SpeakerLabel> = segs.iter().map(|s| s.speaker.clone()).collect();
        assert_eq!(
            who,
            vec![
                SpeakerLabel::Cluster { index: 1 },
                SpeakerLabel::Cluster { index: 2 },
                SpeakerLabel::Cluster { index: 1 },
                SpeakerLabel::Cluster { index: 3 },
                SpeakerLabel::Cluster { index: 3 },
                SpeakerLabel::Cluster { index: 102 },
                SpeakerLabel::Unknown,
            ]
        );
        assert_eq!((segs[0].start_ms, segs[0].end_ms), (900_000, 905_000));
        assert_eq!(segs[5].origin, sys);
    }

    #[test]
    fn empty_live_transcript_preserves_distinct_speakers_per_stream() {
        let lines = vec![
            heard(AudioOrigin::Microphone, 0, 2_000, "说话人1"),
            heard(AudioOrigin::Microphone, 2_500, 4_000, "说话人2"),
            heard(AudioOrigin::System, 4_500, 6_000, "说话人1"),
            heard(AudioOrigin::Microphone, 6_500, 7_000, ""),
        ];
        let segs = heard_to_segments(MeetingId::new(), 0, &lines, &[], Some("zh".into()));
        let who: Vec<SpeakerLabel> = segs.iter().map(|s| s.speaker.clone()).collect();
        assert_eq!(
            who,
            vec![
                SpeakerLabel::Cluster { index: 1 },
                SpeakerLabel::Cluster { index: 2 },
                SpeakerLabel::Cluster { index: 101 },
                SpeakerLabel::Unknown,
            ]
        );
    }

    #[test]
    fn wav_round_trip_skips_padding_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        let samples: Vec<i16> = (0..1600)
            .map(|i| if i % 2 == 0 { 3277 } else { -3277 })
            .collect();
        write_wav_mono16(&path, 16_000, &samples).unwrap();
        // Insert an odd-sized FLLR chunk before `data`, as afconvert does.
        let mut bytes = std::fs::read(&path).unwrap();
        let mut fllr = b"FLLR".to_vec();
        fllr.extend_from_slice(&3u32.to_le_bytes());
        fllr.extend_from_slice(&[0, 0, 0, 0]); // 3 bytes + 1 pad
        bytes.splice(36..36, fllr);
        std::fs::write(&path, &bytes).unwrap();
        let (rate, back) = read_wav_mono16(&path).unwrap();
        assert_eq!((rate, back), (16_000, samples));
        assert!(read_wav_mono16(&dir.path().join("missing.wav")).is_err());
    }

    #[test]
    fn dbfs_and_split_ranges_handle_silence_and_boundaries() {
        assert_eq!(dbfs(&[]), -180.0);
        assert_eq!(dbfs(&[0; 160]), -180.0);
        assert!(dbfs(&[16_384; 160]) > -10.0);
        assert!(split_ranges(0, 100).is_empty());
        assert_eq!(split_ranges(250, 100), vec![0..100, 100..200, 200..250]);
    }

    #[test]
    fn find_speech_spans_drops_silence_splits_pauses_and_bounds_long_monologues() {
        // Pure digital silence produces zero spans.
        assert!(find_speech_spans(&vec![0i16; 32_000], RATE, DEFAULT_UTTERANCE_MS).is_empty());

        // Two bursts separated by 1.2 s silence produce two distinct spans.
        let mut audio = vec![0i16; 8_000]; // 0.5 s silence
        audio.extend(vec![4_000i16; 16_000]); // 1.0 s speech
        audio.extend(vec![0i16; 19_200]); // 1.2 s silence (> 600 ms)
        audio.extend(vec![4_000i16; 16_000]); // 1.0 s speech
        audio.extend(vec![0i16; 8_000]); // 0.5 s silence
        let spans = find_speech_spans(&audio, RATE, DEFAULT_UTTERANCE_MS);
        assert_eq!(spans.len(), 2, "got spans: {spans:?}");
        assert!(spans[0].start < 8_000 && spans[0].end >= 24_000);
        assert!(spans[1].start > spans[0].end);

        // Long continuous monologue exceeding max_span_ms gets split at quietest dip.
        let mut long = vec![5_000i16; RATE as usize * 14];
        // Insert a 200 ms dip at 10.5 s inside [4 s..12 s].
        let dip_start = RATE as usize * 10 + RATE as usize / 2;
        for s in &mut long[dip_start..dip_start + RATE as usize / 5] {
            *s = 100;
        }
        let bounded = find_speech_spans(&long, RATE, 12_000);
        assert_eq!(bounded.len(), 2, "got bounded: {bounded:?}");
        assert!(
            bounded[0].end >= dip_start && bounded[0].end <= dip_start + RATE as usize / 5,
            "split should land in the dip: {:?}",
            bounded[0]
        );
    }

    #[test]
    fn prepare_chunks_encode_active_m4a_and_drop_silent_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let mic = dir.path().join("mic.wav");
        let sys = dir.path().join("sys.wav");
        let mut speech = vec![0i16; 8_000];
        speech.extend(vec![3_000i16; 16_000]);
        speech.extend(vec![0i16; 8_000]);
        write_wav_mono16(&mic, RATE, &speech).unwrap();
        write_wav_mono16(&sys, RATE, &vec![0i16; 32_000]).unwrap();

        let track_work = dir.path().join("track-work");
        let track_chunks = prepare_track_chunks(
            &[
                (AudioOrigin::Microphone, mic.clone()),
                (AudioOrigin::System, sys.clone()),
            ],
            &track_work,
            CHUNK_MS,
        )
        .unwrap();
        assert_eq!(track_chunks.len(), 1);
        assert_eq!(track_chunks[0].origin, AudioOrigin::Microphone);
        assert!(track_chunks[0].path.is_file());

        let utt_work = dir.path().join("utt-work");
        let utt_chunks = prepare_utterance_chunks(
            &[(AudioOrigin::Microphone, mic), (AudioOrigin::System, sys)],
            &utt_work,
            DEFAULT_UTTERANCE_MS,
        )
        .unwrap();
        assert_eq!(utt_chunks.len(), 1);
        assert_eq!(utt_chunks[0].origin, AudioOrigin::Microphone);
        assert!(utt_chunks[0].path.is_file());
    }
}
