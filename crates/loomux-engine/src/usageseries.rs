//! The persisted usage **time series** — the file the token time-plot reads
//! (#2011 slice B).
//!
//! # Why a file at all
//!
//! `usage.json` is cumulative-to-now per CLI session and carries no history:
//! it answers "what has this group spent" and cannot answer "when". The audit
//! log carries no usage rows either (the only usage-touching action is
//! `usage-corrupt`), and it rotates at 8 MB, so it is the wrong file to keep
//! history in. Deriving a series from the CLIs' own transcripts at read time is
//! the cost the scorecard already pays once per run (a long-lived orchestrator
//! transcript reaches hundreds of MB), and opencode keeps no timestamped
//! per-message record loomux reads at all. So the app **samples** what it has
//! already computed: `compute_group_usage` refreshes every live agent's
//! snapshot on its own tick, and one compare per key turns that into a row.
//!
//! # The shape, and why it is cumulative
//!
//! Every [`Sample`] carries the snapshot's counters **as of `ts_ms`**, not the
//! delta since the previous row. That is the property that makes a torn or
//! missing row harmless: a reader differences consecutive rows per key
//! ([`diff_series`]), so a row that never landed costs resolution and never
//! double-counts, where a delta-encoded series would lose that spend forever.
//! It is also what lets the file be append-only with **one writer** (the view
//! publisher thread) and no rotation — see `append_series_line` in `src-tauri`,
//! which is `append_ledger_line`'s single-`write_all` shape for that reason.
//!
//! # What this module is NOT
//!
//! It is not a rebuild path. The series is never reconstructed from
//! `audit.jsonl` or from a transcript: history starts when the build that
//! writes it was first run against a group, and the panel says so rather than
//! drawing a line that pretends otherwise. [`diff_series`] enforces the honest
//! half of that — the FIRST row for a key is a baseline and yields no delta,
//! because its counters are lifetime-to-that-instant and charging them to one
//! five-minute bucket would draw a spike that never happened.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Minimum spacing between two rows for one key, in ms (5 minutes).
///
/// A **spacing**, deliberately not a wall-clock grid: bucketing by
/// `ts_ms / BUCKET` would write two rows ten seconds apart whenever a tick
/// straddles a boundary, which is the one case a budget on this file cares
/// about. The projection re-buckets on read, so the writer's only job is to
/// bound how often it writes.
pub const SERIES_BUCKET_MS: u64 = 5 * 60 * 1000;

/// The tuning fingerprint: component name -> hash (or, for `version`, the
/// literal version string). A map rather than a struct because this crate does
/// not know which surfaces `src-tauri`'s `tuningfp` chooses to hash, and a row
/// written by a later build that hashes one more surface must still parse.
pub type Fp = BTreeMap<String, String>;

/// One row of `<group>/usage-series.jsonl`.
///
/// Internally tagged on `kind`, so the JSON is flat — `{"ts_ms":…,
/// "kind":"sample", "key":…}` — and a reader can branch on `kind` without
/// knowing the rest of the schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SeriesRow {
    Sample(Sample),
    Mark(Mark),
}

impl SeriesRow {
    pub fn ts_ms(&self) -> u64 {
        match self {
            SeriesRow::Sample(s) => s.ts_ms,
            SeriesRow::Mark(m) => m.ts_ms,
        }
    }
}

/// A usage sample for one key, with **cumulative** counters as of `ts_ms`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub ts_ms: u64,
    /// The `UsageSnapshot` key: the CLI session id, else `agent:<id>`.
    pub key: String,
    /// The agent occupying that key **at write time**. `usage.json` keeps the
    /// last occupant only; the series keeps every one, in order.
    pub agent: String,
    /// The workflow block (`worker-std`, `rev-final`, …). Empty on a row
    /// written before the field existed — never guessed.
    #[serde(default)]
    pub block: String,
    /// The CLI that block runs (`claude`, `opencode`, `pi`, …). Empty on a
    /// pre-field row, and never derived from `source`: the two answer
    /// different questions, and `source` has `statusline`/`none` values off
    /// which no CLI can be read at all.
    #[serde(default)]
    pub cli: String,
    pub role: String,
    #[serde(rename = "in")]
    pub input: u64,
    #[serde(rename = "out")]
    pub output: u64,
    pub cache_w: u64,
    pub cache_r: u64,
    pub cost_usd: Option<f64>,
    pub estimated: bool,
    pub source: String,
    pub model: Option<String>,
}

impl Sample {
    /// The four counters summed — what "has anything been counted yet" asks.
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_w)
            .saturating_add(self.cache_r)
    }

    fn counters_differ(&self, other: &Sample) -> bool {
        self.input != other.input
            || self.output != other.output
            || self.cache_w != other.cache_w
            || self.cache_r != other.cache_r
    }
}

/// A tuning **mark**: the moment the repo's agent-facing configuration changed.
///
/// The projection draws these as verticals so a before/after readout can be
/// taken across one. `changed` is derived from `fp` vs `prev` at write time and
/// stored, so a reader need not re-derive it from two maps whose key set may
/// have grown between the builds that wrote them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub ts_ms: u64,
    /// Component names whose hash differs from `prev`, sorted. Includes a
    /// component present on one side only.
    pub changed: Vec<String>,
    pub fp: Fp,
    /// The previous mark's fingerprint, or an empty map for the first mark of
    /// a group's life.
    #[serde(default)]
    pub prev: Fp,
    /// The fingerprint walk hit one of its caps (a file over the size limit, a
    /// directory past the depth limit), so `fp` describes **most** of the
    /// surface rather than all of it. A mark is still worth writing — a change
    /// it did see is still a real change — but a reader must not read an
    /// unchanged component as proof that nothing under it moved.
    #[serde(default)]
    pub fp_partial: bool,
}

/// Component names whose value differs between two fingerprints, sorted.
///
/// A component present on one side only counts as changed: that is the shape a
/// build which hashes a new surface produces, and calling it "unchanged" would
/// hide the very first mark after such an upgrade.
pub fn fp_changed(prev: &Fp, now: &Fp) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in now {
        if prev.get(k) != Some(v) {
            out.push(k.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Whether `now` is worth appending, given the last row written for that key.
///
/// Two conditions, and **both** must hold:
///
/// 1. **A counter moved.** An agent idle for an hour writes nothing; its line
///    in the plot is flat because the row before it says so, not because sixty
///    identical rows say it. With no previous row, "moved" means the snapshot
///    has actually counted something — a freshly spawned pane whose transcript
///    is still empty is not a data point, and writing it would put a
///    `source: "none"` zero row at the head of every key.
/// 2. **The bucket elapsed.** At most one row per key per
///    [`SERIES_BUCKET_MS`], measured from the last row WRITTEN — see that
///    constant for why this is a spacing rather than a grid.
///
/// A `ts_ms` that goes backwards (a wall-clock correction between two ticks)
/// counts as elapsed: the alternative is a key that stops sampling until the
/// clock catches up, which is a silent hole exactly when the timestamps are
/// already untrustworthy.
pub fn should_sample(prev: Option<&Sample>, now: &Sample, bucket_ms: u64) -> bool {
    let Some(prev) = prev else {
        return now.total() > 0;
    };
    if !prev.counters_differ(now) {
        return false;
    }
    now.ts_ms < prev.ts_ms || now.ts_ms.saturating_sub(prev.ts_ms) >= bucket_ms
}

/// Parse `usage-series.jsonl` text into rows, oldest first, skipping
/// unparseable lines — the same per-line fault tolerance `parse_audit_lines`
/// gives the audit log, and for the same reason: one torn tail (a reader
/// racing the appender) must cost one row, never the file.
pub fn parse_series_lines(text: &str) -> Vec<SeriesRow> {
    parse_series_lines_counted(text).0
}

/// Same, and also how many non-blank lines failed to parse. Blank lines do not
/// count: a trailing newline is normal. Unparseable *content* is not, and a
/// caller that silently drops it turns a corrupt file into a slightly shorter
/// chart with nothing anywhere saying so (#240's lesson, applied here before it
/// can happen).
pub fn parse_series_lines_counted(text: &str) -> (Vec<SeriesRow>, usize) {
    let mut skipped = 0usize;
    let rows = text
        .lines()
        .filter_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            match serde_json::from_str::<SeriesRow>(line) {
                Ok(r) => Some(r),
                Err(_) => {
                    skipped += 1;
                    None
                }
            }
        })
        .collect();
    (rows, skipped)
}

/// One differenced interval for a key: what was spent BETWEEN two consecutive
/// samples.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    /// The later sample's timestamp — the bucket this spend is charged to.
    pub ts_ms: u64,
    /// The earlier sample's timestamp, so a reader can see the interval's width
    /// and refuse to draw a slope across a long gap.
    pub from_ts_ms: u64,
    pub key: String,
    pub agent: String,
    pub block: String,
    pub cli: String,
    pub role: String,
    #[serde(rename = "in")]
    pub input: u64,
    #[serde(rename = "out")]
    pub output: u64,
    pub cache_w: u64,
    pub cache_r: u64,
    /// Dollar delta, or `None` when either endpoint had no figure — never 0.0,
    /// which would read as "this interval cost nothing".
    pub cost_usd: Option<f64>,
    /// A cumulative counter went DOWN across this interval, so the deltas are
    /// clamped to 0 rather than wrapped. The causes are ordinary — the usage
    /// cursor revalidates every `CURSOR_REVALIDATE_AFTER` and a rotated or
    /// truncated transcript re-reads shorter — so this is a labelled data
    /// point, not an error.
    pub reset: bool,
}

/// Difference consecutive samples per key.
///
/// **The first row for a key is a baseline and produces no delta.** Its
/// counters are that session's lifetime up to the moment sampling began, which
/// may be an hour of spend the series never saw; charging it to the first
/// interval would draw a spike that did not happen. The panel's coverage note
/// ("series since <ts>") is the other half of that statement.
///
/// Never negative: a shrunk counter yields 0 for that component and sets
/// [`Delta::reset`] on the row.
pub fn diff_series(rows: &[SeriesRow]) -> Vec<Delta> {
    let mut prev: BTreeMap<String, Sample> = BTreeMap::new();
    let mut out: Vec<Delta> = Vec::new();
    for row in rows {
        let SeriesRow::Sample(s) = row else { continue };
        let Some(p) = prev.insert(s.key.clone(), s.clone()) else {
            continue; // baseline
        };
        let mut reset = false;
        let sub = |a: u64, b: u64, reset: &mut bool| -> u64 {
            if a < b {
                *reset = true;
                0
            } else {
                a - b
            }
        };
        let input = sub(s.input, p.input, &mut reset);
        let output = sub(s.output, p.output, &mut reset);
        let cache_w = sub(s.cache_w, p.cache_w, &mut reset);
        let cache_r = sub(s.cache_r, p.cache_r, &mut reset);
        let cost_usd = match (s.cost_usd, p.cost_usd) {
            (Some(a), Some(b)) => {
                if a < b {
                    reset = true;
                    Some(0.0)
                } else {
                    Some(a - b)
                }
            }
            _ => None,
        };
        out.push(Delta {
            ts_ms: s.ts_ms,
            from_ts_ms: p.ts_ms,
            key: s.key.clone(),
            agent: s.agent.clone(),
            block: s.block.clone(),
            cli: s.cli.clone(),
            role: s.role.clone(),
            input,
            output,
            cache_w,
            cache_r,
            cost_usd,
            reset,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts_ms: u64, key: &str, input: u64) -> Sample {
        Sample {
            ts_ms,
            key: key.to_string(),
            agent: "w-1".to_string(),
            block: "worker".to_string(),
            cli: "claude".to_string(),
            role: "worker".to_string(),
            input,
            output: 0,
            cache_w: 0,
            cache_r: 0,
            cost_usd: None,
            estimated: false,
            source: "transcript".to_string(),
            model: None,
        }
    }

    #[test]
    fn a_sample_is_taken_only_when_a_counter_moved_and_the_bucket_elapsed() {
        let prev = sample(1_000_000, "s1", 100);

        // Both conditions met — the discriminating positive, so the refusals
        // below are not merely "always false".
        let moved_and_late = sample(1_000_000 + SERIES_BUCKET_MS, "s1", 101);
        assert!(should_sample(Some(&prev), &moved_and_late, SERIES_BUCKET_MS));

        // The bucket elapsed but NOTHING moved — an idle agent writes no row.
        let mut idle = moved_and_late.clone();
        idle.input = prev.input;
        assert!(
            !should_sample(Some(&prev), &idle, SERIES_BUCKET_MS),
            "an idle agent must not write a row every bucket"
        );

        // A counter moved but the bucket has NOT elapsed — the app's usage tick
        // is ~1 s, so without this the file would grow once a second per key.
        let mut early = moved_and_late.clone();
        early.ts_ms = prev.ts_ms + 1_000;
        assert!(
            !should_sample(Some(&prev), &early, SERIES_BUCKET_MS),
            "a moved counter one second later must not write a row"
        );

        // No previous row: the first sample lands only once something has been
        // counted, so a freshly spawned pane does not seed every key with a
        // zero row.
        assert!(should_sample(None, &sample(1, "s1", 1), SERIES_BUCKET_MS));
        assert!(
            !should_sample(None, &sample(1, "s1", 0), SERIES_BUCKET_MS),
            "a zero-usage snapshot is not a data point"
        );

        // A wall-clock correction must not wedge a key: backwards counts as
        // elapsed.
        let mut backwards = sample(prev.ts_ms - 60_000, "s1", 200);
        backwards.output = 5;
        assert!(should_sample(Some(&prev), &backwards, SERIES_BUCKET_MS));
    }

    #[test]
    fn every_counter_can_move_a_sample_on_its_own() {
        // The "did anything move" comparison reads FOUR counters, and a
        // one-counter fixture would leave three of them unpinned — the axis a
        // `self.input != other.input` typo would silently narrow it to.
        let prev = sample(0, "s1", 100);
        for (i, mutate) in [
            (|s: &mut Sample| s.input += 1) as fn(&mut Sample),
            |s: &mut Sample| s.output += 1,
            |s: &mut Sample| s.cache_w += 1,
            |s: &mut Sample| s.cache_r += 1,
        ]
        .into_iter()
        .enumerate()
        {
            let mut now = sample(SERIES_BUCKET_MS, "s1", 100);
            mutate(&mut now);
            assert!(
                should_sample(Some(&prev), &now, SERIES_BUCKET_MS),
                "counter {i} alone must be enough to move a sample"
            );
        }
        // The negative control for the same loop: nothing moved at all.
        let still = sample(SERIES_BUCKET_MS, "s1", 100);
        assert!(!should_sample(Some(&prev), &still, SERIES_BUCKET_MS));
    }

    #[test]
    fn a_torn_last_line_is_skipped_not_fatal() {
        let good = serde_json::to_string(&SeriesRow::Sample(sample(10, "s1", 5))).unwrap();
        let later = serde_json::to_string(&SeriesRow::Sample(sample(20, "s1", 9))).unwrap();
        // A reader racing the appender sees the tail cut mid-object.
        let torn = &good[..good.len() - 12];
        let text = format!("{good}\n{torn}\n{later}\n\n");

        let (rows, skipped) = parse_series_lines_counted(&text);
        assert_eq!(rows.len(), 2, "both whole rows survive: {rows:?}");
        assert_eq!(skipped, 1, "and the torn one is COUNTED, not silently dropped");
        assert_eq!(rows[0].ts_ms(), 10);
        assert_eq!(rows[1].ts_ms(), 20);
        // The blank trailing line is normal and must not be counted as damage.
        let (kept, none) = parse_series_lines_counted(&format!("{good}\n\n"));
        assert_eq!((kept.len(), none), (1, 0));
    }

    #[test]
    fn diffs_never_go_negative_on_a_cursor_reset() {
        let mut a = sample(1_000, "s1", 500);
        a.output = 50;
        a.cost_usd = Some(1.50);
        // The cursor revalidated against a rotated transcript and re-read
        // SHORTER — every counter drops.
        let mut b = sample(2_000, "s1", 10);
        b.output = 1;
        b.cost_usd = Some(0.10);
        // Then it grows again from the new baseline.
        let mut c = sample(3_000, "s1", 40);
        c.output = 4;
        c.cost_usd = Some(0.40);

        let deltas = diff_series(&[SeriesRow::Sample(a), SeriesRow::Sample(b), SeriesRow::Sample(c)]);
        assert_eq!(deltas.len(), 2, "the first row is a baseline: {deltas:?}");

        let shrunk = &deltas[0];
        assert_eq!((shrunk.input, shrunk.output), (0, 0), "clamped, never wrapped");
        assert_eq!(shrunk.cost_usd, Some(0.0));
        assert!(shrunk.reset, "and the clamp is LABELLED, not silent");

        let grew = &deltas[1];
        assert_eq!((grew.input, grew.output), (30, 3));
        let cost = grew.cost_usd.expect("both endpoints priced");
        assert!((cost - 0.30).abs() < 1e-9, "cost delta was {cost}");
        assert!(!grew.reset, "a normal interval carries no reset flag");
        assert_eq!(grew.from_ts_ms, 2_000, "the interval's own width is reported");

        // An unpriced endpoint yields `None`, never 0.0 — "no figure" and
        // "cost nothing" are different answers.
        let mut d = sample(4_000, "s1", 60);
        d.cost_usd = None;
        let unpriced = diff_series(&[SeriesRow::Sample(sample(3_500, "s1", 50)), SeriesRow::Sample(d)]);
        assert_eq!(unpriced[0].cost_usd, None);
    }

    #[test]
    fn a_baseline_row_is_not_charged_and_keys_are_differenced_independently() {
        // Two keys interleaved: each gets its own baseline, and neither key's
        // counters leak into the other's delta. The literals COLLIDE on
        // purpose — disjoint magnitudes would hold under an implementation
        // that shared one running total.
        let rows = vec![
            SeriesRow::Sample(sample(1_000, "s1", 100)),
            SeriesRow::Sample(sample(1_100, "s2", 7_000)),
            SeriesRow::Sample(sample(2_000, "s1", 150)),
            SeriesRow::Sample(sample(2_100, "s2", 7_005)),
        ];
        let deltas = diff_series(&rows);
        assert_eq!(deltas.len(), 2, "two baselines, two intervals: {deltas:?}");
        assert_eq!(deltas[0].key, "s1");
        assert_eq!(deltas[0].input, 50, "not 150 — the baseline is not charged");
        assert_eq!(deltas[1].key, "s2");
        assert_eq!(deltas[1].input, 5, "s2's much larger baseline never reaches s1");
        assert!(!deltas.iter().any(|d| d.reset), "nothing here shrank");
    }

    #[test]
    fn a_mark_is_emitted_only_on_fingerprint_change() {
        let mut a: Fp = BTreeMap::new();
        a.insert("workflow".into(), "aaa".into());
        a.insert("claude_md".into(), "bbb".into());
        a.insert("version".into(), "1.3.0".into());

        assert!(fp_changed(&a, &a).is_empty(), "an identical fingerprint is not a mark");

        let mut b = a.clone();
        b.insert("workflow".into(), "zzz".into());
        assert_eq!(fp_changed(&a, &b), vec!["workflow".to_string()], "only that component");

        // A build that hashes one MORE surface must not report the new
        // component as unchanged — the first mark after such an upgrade is
        // exactly the one a reader needs to see.
        let mut c = a.clone();
        c.insert("lessons".into(), "ccc".into());
        assert_eq!(fp_changed(&a, &c), vec!["lessons".to_string()]);
        // And the reverse direction (a component that went away) counts too.
        assert_eq!(fp_changed(&c, &a), vec!["lessons".to_string()]);

        // The very first mark of a group's life diffs against an empty map and
        // reports every component, which is what makes it a mark at all.
        let empty: Fp = BTreeMap::new();
        assert_eq!(fp_changed(&empty, &a).len(), 3);
    }

    #[test]
    fn the_wire_form_is_flat_and_a_pre_block_row_still_loads() {
        let row = SeriesRow::Sample(sample(42, "s1", 3));
        let text = serde_json::to_string(&row).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["kind"], "sample", "the tag is flat, not nested: {text}");
        assert_eq!(v["ts_ms"], 42);
        assert_eq!(v["in"], 3, "the counters use the persisted short names: {text}");

        // Additive: a row written before `block`/`cli` existed loads with them
        // EMPTY rather than failing the whole file — and empty is a value the
        // projection can label "unknown", where a guessed default would put
        // real spend under the wrong block.
        let legacy = r#"{"ts_ms":1,"kind":"sample","key":"s1","agent":"w-1","role":"worker","in":1,"out":2,"cache_w":0,"cache_r":0,"cost_usd":null,"estimated":false,"source":"transcript","model":null}"#;
        let (rows, skipped) = parse_series_lines_counted(legacy);
        assert_eq!(skipped, 0, "a pre-field row is not damage");
        let SeriesRow::Sample(s) = &rows[0] else {
            panic!("expected a sample")
        };
        assert_eq!((s.block.as_str(), s.cli.as_str()), ("", ""));

        let mark = SeriesRow::Mark(Mark {
            ts_ms: 7,
            changed: vec!["workflow".into()],
            fp: BTreeMap::new(),
            prev: BTreeMap::new(),
            fp_partial: true,
        });
        let mv: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&mark).unwrap()).unwrap();
        assert_eq!(mv["kind"], "mark");
        assert_eq!(mv["fp_partial"], true);
    }
}
