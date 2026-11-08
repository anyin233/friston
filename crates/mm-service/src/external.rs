//! Post-meeting transcription by an external speech model (Gemini via `agy`).
//!
//! Two pure halves, so the orchestration in the app stays thin:
//!
//! * [`prepare_recordings`] turns the engine's `.caf` files into 16 kHz mono AAC parts of at
//!   most [`CHUNK_MS`]. Long parts make the model's single JSON answer too long to finish
//!   reliably; 10 minutes took 52 s and ~18k output tokens in testing. Silent tracks are
//!   dropped: given a digitally silent system-audio file, a model can invent a whole meeting.
//! * [`heard_to_segments`] maps the model's sentences onto this meeting's speakers. The
//!   model's labels ("说话人1") only hold within one run, while the live diarization knows
//!   enrolled people and keeps one hue per person, so identity comes from the live pass.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;

use mm_core::{AudioOrigin, MeetingId, SpeakerLabel, TranscriptSegment};
use mm_llm::antigravity::{AudioTrack, HeardLine};

use crate::ServiceError;

/// Longest audio part sent in one run.
pub const CHUNK_MS: u64 = 15 * 60 * 1000;

/// Tracks quieter than this (RMS, dBFS) are treated as silent and not sent. Room noise on
/// the built-in mic sits around −57 dBFS; a muted system-audio tap is digital zero.
pub const SILENCE_DBFS: f64 = -70.0;

/// Sample rate of the prepared audio.
const RATE: u32 = 16_000;

/// One part of the meeting, ready for a model run.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// Position of this part from the start of the recording.
    pub offset_ms: u64,
    /// Working directory of the run: holds exactly `tracks`.
    pub dir: PathBuf,
    pub tracks: Vec<AudioTrack>,
}

/// File stem used for a track.
fn stem(origin: AudioOrigin) -> &'static str {
    match origin {
        AudioOrigin::Microphone => "microphone",
        AudioOrigin::System => "system",
    }
}

/// Runs macOS `afconvert`.
///
/// Params: `args` – arguments after the program name.
/// Returns: `Ok(())` when it exits 0.
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

/// Converts recordings into silent-free, size-limited parts under `work`.
///
/// Params: `files` – `(origin, recording)` pairs; `work` – scratch folder, emptied first.
/// Returns: the parts that contain sound, in time order.
pub fn prepare_recordings(
    files: &[(AudioOrigin, PathBuf)],
    work: &Path,
) -> Result<Vec<Chunk>, ServiceError> {
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
    let longest = decoded.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
    let per_chunk = (CHUNK_MS * RATE as u64 / 1000) as usize;
    let mut chunks = Vec::new();
    for (i, range) in split_ranges(longest, per_chunk).into_iter().enumerate() {
        let dir = work.join(format!("part-{i:02}"));
        let mut tracks = Vec::new();
        for (origin, samples) in &decoded {
            let slice = &samples[range.start.min(samples.len())..range.end.min(samples.len())];
            if dbfs(slice) < SILENCE_DBFS {
                continue;
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| ServiceError::External(format!("creating {}: {e}", dir.display())))?;
            let wav = dir.join(format!("{}.wav", stem(*origin)));
            let m4a = dir.join(format!("{}.m4a", stem(*origin)));
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
            tracks.push(AudioTrack {
                origin: *origin,
                file_name: format!("{}.m4a", stem(*origin)),
            });
        }
        if !tracks.is_empty() {
            chunks.push(Chunk {
                offset_ms: range.start as u64 * 1000 / RATE as u64,
                dir,
                tracks,
            });
        }
    }
    Ok(chunks)
}

/// Splits `len` samples into consecutive ranges of at most `per_chunk`.
///
/// Params: `len`; `per_chunk` – must be > 0.
/// Returns: the ranges (empty for `len == 0`).
pub fn split_ranges(len: usize, per_chunk: usize) -> Vec<Range<usize>> {
    (0..len)
        .step_by(per_chunk.max(1))
        .map(|start| start..(start + per_chunk).min(len))
        .collect()
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
    let rms = (sum / samples.len() as f64).sqrt() / 32768.0;
    if rms <= 0.0 {
        -180.0
    } else {
        20.0 * rms.log10()
    }
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

/// Turns one run's sentences into final-pass segments with this meeting's speakers.
///
/// A sentence takes the speaker of the live segment covering most of it (at least half).
/// Otherwise it takes the speaker that the model's label for it overlaps most across the
/// run, and failing that `Unknown`.
///
/// Params: `meeting`; `offset_ms` – the part's start; `heard` – the run's sentences;
/// `live` – the live-pass segments; `language` – caption language.
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
        let key = (l.source, l.speaker.as_str());
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
    heard
        .iter()
        .filter(|l| !l.text.trim().is_empty())
        .map(|l| {
            let (start, end) = span(l);
            let direct = live
                .iter()
                .filter(|s| s.origin == l.source)
                .map(|s| (overlap((start, end), (s.start_ms, s.end_ms)), s))
                .filter(|(ms, _)| *ms > 0 && ms * 2 >= (end - start))
                .max_by_key(|(ms, _)| *ms)
                .map(|(_, s)| s.speaker.clone());
            let speaker = direct
                .or_else(|| by_label((l.source, l.speaker.as_str())))
                .unwrap_or(SpeakerLabel::Unknown);
            TranscriptSegment {
                meeting_id: meeting,
                start_ms: start,
                end_ms: end,
                text: l.text.trim().to_string(),
                language: language.clone(),
                speaker,
                origin: l.source,
                spatial: None,
            }
        })
        .collect()
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
    fn speakers_come_from_live_overlap_then_label_votes() {
        let mic = AudioOrigin::Microphone;
        let live_pass = vec![
            live(mic, 900_000, 905_000, 1),
            live(mic, 906_000, 910_000, 2),
            // Same time, other track: must never be borrowed.
            live(AudioOrigin::System, 900_000, 990_000, 101),
        ];
        let lines = vec![
            heard(mic, 0, 5_000, "说话人1"),       // covered by cluster 1
            heard(mic, 6_000, 9_000, "说话人2"),   // covered by cluster 2
            heard(mic, 30_000, 31_000, "说话人1"), // no live segment: label vote → cluster 1
            heard(mic, 40_000, 41_000, "说话人3"), // nothing known → Unknown
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
                SpeakerLabel::Unknown,
            ]
        );
        assert_eq!((segs[0].start_ms, segs[0].end_ms), (900_000, 905_000));
        assert!(segs.iter().all(|s| s.origin == mic));
    }

    #[test]
    fn wav_round_trip_skips_padding_chunks_and_measures_level() {
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
        assert_eq!((rate, back), (16_000, samples.clone()));
        assert!((dbfs(&samples) - -20.0).abs() < 0.1);
        assert_eq!(dbfs(&[0, 0, 0]), -180.0);
        assert!(read_wav_mono16(&dir.path().join("missing.wav")).is_err());
    }

    #[test]
    fn ranges_cover_everything_once() {
        assert_eq!(split_ranges(0, 10), Vec::<Range<usize>>::new());
        assert_eq!(split_ranges(25, 10), vec![0..10, 10..20, 20..25]);
        assert_eq!(split_ranges(10, 10), vec![0..10]);
    }

    /// Exercises afconvert end to end: silent tracks and silent parts are dropped.
    #[test]
    fn prepare_drops_silent_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let tone: Vec<i16> = (0..16_000 * 2)
            .map(|i| ((i as f64 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let mic = dir.path().join("mic.wav");
        let sys = dir.path().join("system.wav");
        write_wav_mono16(&mic, 16_000, &tone).unwrap();
        write_wav_mono16(&sys, 16_000, &vec![0; tone.len()]).unwrap();
        let work = dir.path().join("work");
        let chunks = prepare_recordings(
            &[(AudioOrigin::Microphone, mic), (AudioOrigin::System, sys)],
            &work,
        )
        .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].offset_ms, 0);
        assert_eq!(chunks[0].tracks.len(), 1);
        assert_eq!(chunks[0].tracks[0].origin, AudioOrigin::Microphone);
        let files: Vec<_> = std::fs::read_dir(&chunks[0].dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        // Only the audio the agent may read is in its working directory.
        assert_eq!(files, vec!["microphone.m4a".to_string()]);
    }
}
