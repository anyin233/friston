//! Time windows for audio/video embedding within EmbeddingGemma 2's context.
//!
//! Token costs (model card): audio 25 tokens/s (≈327 s max), video 140 tokens/frame
//! at the default budget (≈58 frames max), sharing one 8 192-token context.

/// Default audio window: 30 s ≈ 750 tokens, small enough for precise retrieval hits.
pub const AUDIO_WINDOW_MS: u64 = 30_000;
/// Default video window: 20 s sampled at 1 fps = 20 frames ≈ 2 800 tokens.
pub const VIDEO_WINDOW_MS: u64 = 20_000;

/// Splits `[0, duration_ms)` into windows of `window_ms` advancing by `hop_ms`.
///
/// The final window is aligned to the end so the tail is never shorter than a
/// full window (unless the whole media is shorter).
///
/// Params: `duration_ms` – media length; `window_ms` – window size; `hop_ms` – stride (> 0).
/// Returns: `(start_ms, end_ms)` pairs in order.
pub fn windows(duration_ms: u64, window_ms: u64, hop_ms: u64) -> Vec<(u64, u64)> {
    assert!(
        hop_ms > 0 && window_ms > 0,
        "window and hop must be positive"
    );
    if duration_ms <= window_ms {
        return if duration_ms == 0 {
            vec![]
        } else {
            vec![(0, duration_ms)]
        };
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start + window_ms < duration_ms {
        out.push((start, start + window_ms));
        start += hop_ms;
    }
    let tail = (duration_ms - window_ms, duration_ms);
    if out.last() != Some(&tail) {
        out.push(tail);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_media_with_end_aligned_tail() {
        let w = windows(70_000, 30_000, 25_000);
        assert_eq!(w, vec![(0, 30_000), (25_000, 55_000), (40_000, 70_000)]);
        assert_eq!(windows(10_000, 30_000, 25_000), vec![(0, 10_000)]);
        assert!(windows(0, 30_000, 25_000).is_empty());
    }
}
