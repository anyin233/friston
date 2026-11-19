//! Local cloud ASR usage and estimated USD spend ledger.

use chrono::{DateTime, SecondsFormat, Utc};
use mm_core::MeetingId;
use rusqlite::params;

use crate::{Store, StoreError};

/// One persisted cloud ASR usage entry (either a live utterance or a refine job).
#[derive(Debug, Clone, PartialEq)]
pub struct AsrUsageRecord {
    /// Timestamp when the usage occurred (stored in RFC3339 UTC).
    pub recorded_at: DateTime<Utc>,
    /// Optional meeting id (`None` if unknown or after meeting deletion).
    pub meeting_id: Option<MeetingId>,
    /// Cloud provider id (`"openai"` or `"gemini"`).
    pub provider: String,
    /// `"live"` or `"refine"`.
    pub mode: String,
    /// Model id used for the call.
    pub model: String,
    /// Processed audio duration in milliseconds.
    pub audio_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Estimated USD cost for this usage record.
    pub cost_usd: f64,
}

/// Aggregated duration, tokens, call count, and USD spend.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AsrUsageTotals {
    pub audio_ms: u64,
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub calls: u64,
}

impl AsrUsageTotals {
    /// Adds one usage delta in place (used for optimistic UI updates).
    ///
    /// Params: `audio_ms`; `cost_usd`; `input_tokens`; `output_tokens`.
    pub fn add(&mut self, audio_ms: u64, cost_usd: f64, input_tokens: u64, output_tokens: u64) {
        self.audio_ms = self.audio_ms.saturating_add(audio_ms);
        self.cost_usd += cost_usd;
        self.input_tokens = self.input_tokens.saturating_add(input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(output_tokens);
        self.calls = self.calls.saturating_add(1);
    }
}

/// Current-month and all-time local cumulative ASR usage for one provider.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProviderUsageSummary {
    pub month: AsrUsageTotals,
    pub total: AsrUsageTotals,
}

/// Formats a UTC timestamp so SQLite lexicographical ordering matches chronological order.
fn utc_stamp(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Sums rows matching a SQL `WHERE` clause into [`AsrUsageTotals`].
fn query_totals(
    store: &Store,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<AsrUsageTotals, StoreError> {
    Ok(store.conn().query_row(sql, params, |r| {
        let audio_ms: i64 = r.get(0)?;
        let cost_usd: f64 = r.get(1)?;
        let input_tokens: i64 = r.get(2)?;
        let output_tokens: i64 = r.get(3)?;
        let calls: i64 = r.get(4)?;
        Ok(AsrUsageTotals {
            audio_ms: audio_ms.max(0) as u64,
            cost_usd: cost_usd.max(0.0),
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            calls: calls.max(0) as u64,
        })
    })?)
}

impl Store {
    /// Records one cloud ASR usage item in `asr_usage`.
    ///
    /// Params: `rec` – usage entry to persist.
    /// Returns: `Ok(())` once inserted.
    pub fn record_asr_usage(&self, rec: &AsrUsageRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO asr_usage(
                recorded_at, meeting_id, provider, mode, model,
                audio_ms, input_tokens, output_tokens, cost_usd
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                utc_stamp(rec.recorded_at),
                rec.meeting_id.map(|id| id.to_string()),
                rec.provider,
                rec.mode,
                rec.model,
                rec.audio_ms as i64,
                rec.input_tokens as i64,
                rec.output_tokens as i64,
                rec.cost_usd,
            ],
        )?;
        Ok(())
    }

    /// Aggregates local cumulative ASR usage for `provider` (this month since `month_start_utc`
    /// and all-time total).
    ///
    /// Params: `provider` – `"openai"` or `"gemini"`; `month_start_utc` – start of the current
    /// local calendar month converted to UTC.
    /// Returns: aggregated totals for the month and all time.
    pub fn asr_usage_summary(
        &self,
        provider: &str,
        month_start_utc: DateTime<Utc>,
    ) -> Result<ProviderUsageSummary, StoreError> {
        let month_key = utc_stamp(month_start_utc);
        let month = query_totals(
            self,
            "SELECT COALESCE(SUM(audio_ms), 0),
                    COALESCE(SUM(cost_usd), 0.0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COUNT(*)
             FROM asr_usage
             WHERE provider = ?1 AND recorded_at >= ?2",
            params![provider, month_key],
        )?;
        let total = query_totals(
            self,
            "SELECT COALESCE(SUM(audio_ms), 0),
                    COALESCE(SUM(cost_usd), 0.0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COUNT(*)
             FROM asr_usage
             WHERE provider = ?1",
            params![provider],
        )?;
        Ok(ProviderUsageSummary { month, total })
    }

    /// Deletes local ASR usage history (for one provider or all providers).
    ///
    /// Params: `provider` – `Some("openai")` / `Some("gemini")`, or `None` to clear all.
    /// Returns: number of deleted rows.
    pub fn clear_asr_usage(&self, provider: Option<&str>) -> Result<usize, StoreError> {
        let rows = match provider {
            Some(p) => self
                .conn
                .execute("DELETE FROM asr_usage WHERE provider = ?1", [p])?,
            None => self.conn.execute("DELETE FROM asr_usage", [])?,
        };
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn usage_records_aggregate_by_provider_month_and_survive_meeting_delete() {
        let store = Store::open_in_memory().unwrap();
        let m = crate::new_meeting("周会", Utc::now());
        store.upsert_meeting(&m).unwrap();

        let oct = Utc.with_ymd_and_hms(2026, 10, 20, 8, 0, 0).unwrap();
        let nov_start = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let nov_mid = Utc.with_ymd_and_hms(2026, 11, 19, 12, 0, 0).unwrap();

        store
            .record_asr_usage(&AsrUsageRecord {
                recorded_at: oct,
                meeting_id: Some(m.id),
                provider: "openai".into(),
                mode: "refine".into(),
                model: "gpt-transcribe".into(),
                audio_ms: 60_000,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0045,
            })
            .unwrap();
        store
            .record_asr_usage(&AsrUsageRecord {
                recorded_at: nov_mid,
                meeting_id: Some(m.id),
                provider: "openai".into(),
                mode: "live".into(),
                model: "gpt-live-transcribe".into(),
                audio_ms: 30_000,
                input_tokens: 500,
                output_tokens: 80,
                cost_usd: 0.0085,
            })
            .unwrap();
        store
            .record_asr_usage(&AsrUsageRecord {
                recorded_at: nov_mid,
                meeting_id: None,
                provider: "gemini".into(),
                mode: "live".into(),
                model: "gemini-3.5-transcribe-live".into(),
                audio_ms: 120_000,
                input_tokens: 3000,
                output_tokens: 350,
                cost_usd: 0.01785,
            })
            .unwrap();

        // Deleting the meeting preserves the spend history (ON DELETE SET NULL).
        store
            .conn()
            .execute("DELETE FROM meetings WHERE id = ?1", [m.id.to_string()])
            .unwrap();

        let oa = store.asr_usage_summary("openai", nov_start).unwrap();
        assert_eq!(oa.month.audio_ms, 30_000);
        assert_eq!(oa.month.calls, 1);
        assert!((oa.month.cost_usd - 0.0085).abs() < 1e-9);
        assert_eq!(oa.total.audio_ms, 90_000);
        assert_eq!(oa.total.calls, 2);
        assert!((oa.total.cost_usd - 0.013).abs() < 1e-9);

        let gm = store.asr_usage_summary("gemini", nov_start).unwrap();
        assert_eq!(gm.month.audio_ms, 120_000);
        assert_eq!(gm.month.input_tokens, 3000);
        assert_eq!(gm.total.calls, 1);

        assert_eq!(store.clear_asr_usage(Some("openai")).unwrap(), 2);
        assert_eq!(
            store.asr_usage_summary("openai", nov_start).unwrap(),
            ProviderUsageSummary::default()
        );
        assert_eq!(
            store
                .asr_usage_summary("gemini", nov_start)
                .unwrap()
                .total
                .calls,
            1
        );
        assert_eq!(store.clear_asr_usage(None).unwrap(), 1);
    }
}
