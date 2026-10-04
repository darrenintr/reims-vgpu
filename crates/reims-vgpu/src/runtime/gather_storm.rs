//! The census of one second's guest-run gathers.
//!
//! `gw_*` counts what the witness decided and `sampled_gather_*` counts what the
//! engine then did, and neither can say *why* the hypervisor half had no answer
//! for a window or which windows a burst was made of. A driven RX 7600 boot with
//! the diagnostic witness off read one second at 628 gathers and 1.60 GB while
//! `gw_unarmed=610`, `gw_rearm=15`, `gw_vouched=18` and `gw_hw_quiet=624` — a
//! workload the device's own write record had no objection to, refused almost
//! entirely because the dirty tracker had not answered. This module splits that
//! second without a line per bind.
//!
//! Measuring is all it does. Nothing here is read back to decide a bind, or what
//! the sampled cache admits.
//!
//! # Two tiers, because the cost differs by three orders of magnitude
//!
//! **Always on: relaxed atomic counters, no lock, no allocation.** One
//! `note_gather` is a few dozen relaxed additions on words only the drain thread
//! writes. They feed three lines, emitted once per census second and only when
//! something happened in it:
//!
//! | line | what it divides |
//! |---|---|
//! | `gather_storm` | the second's binds: vouched, re-pointed (and why), unarmed (and why), refused; the kilobytes each cost |
//! | `gather_storm_ready` | how long a window took to read its first generation: binds, tranches, microseconds |
//! | `gather_storm_evict` | what the sampled cache threw away: was it ever hit, was a newer generation of its window already there |
//!
//! **Opt-in: the per-window table** ([`crate::config::GATHER_STORM_KEYS`]). One
//! lock and one hash-map update per bind, bounded to `ROWS_MAX` windows a second.
//! It adds `gather_storm_keys`, the heaviest windows by kilobytes the witness did
//! not vouch for, with per-window unarmed run, tranche count and evictions. Off,
//! none of that exists: no lock is taken and the table is never built.
//!
//! # Reading it
//!
//! `unarmed` counts binds of a window whose pages did not move since the previous
//! bind, so a repeat of an identical window by construction; `rearmed` counts the
//! genuinely new or re-pointed ones. The three `unarmed_*` causes divide
//! `unarmed`: `untracked` is a host that refused the token (no number of harvests
//! helps), `arming` is a token inside its harvest window (every bind gathers),
//! `no_baseline` is the one bind after arming whose gather the baseline describes
//! and which is therefore not wasted. `unreadable` is every bind that read no
//! generation at all — the population whose retained image no later bind can
//! name — whatever its verdict.
//!
//! `unarmed_same_tranche` against `unarmed` says whether the repeats were inside
//! one drain tranche. That is the nearest thing the Rust side can name to "no
//! harvest between them", because harvests are driven by the doorbells a tranche
//! drains; it is not a count of harvests, which the shim does not expose.
//!
//! `fold_same` / `fold_moved` exist only under
//! [`crate::config::GATHER_STORM_FOLD`]: they say whether the bytes an unarmed
//! re-gather moved were the bytes the previous gather moved. They are the ceiling
//! on what any content-based reuse in the arming window could save, and a boot
//! that reads them is not a timing boot.
//!
//! # What it costs
//!
//! `cargo test --release -- --ignored census_cost` prices both tiers on the host
//! it runs on; the figures belong to that run and are not carried here. The rate
//! to price against is `gw_rail_*`: hundreds of binds a second on the worst
//! second this was built for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use parking_lot::Mutex;

use super::gather_witness::{
    EntryLife, GatherKey, GatherObservation, GatherRail, GatherVerdict, RearmWhy, ShadowFold,
    UnarmedCause,
};

/// Distinct windows the opt-in table tracks in one census second.
///
/// A bound on this instrument's memory, not on the workload: the witness tracks
/// at most 256 windows, so a second that names more than eight times that is a
/// key that does not repeat, and `rows_dropped` says so.
const ROWS_MAX: usize = 2048;

/// Windows named on `gather_storm_keys`.
///
/// A cut and not a size: `windows=` on that line is the true count, and
/// `top_share_pct` says how much of the unvouched volume the named ones carry.
const KEYS_REPORTED: usize = 6;

fn kb(span: u64) -> u64 {
    span / 1024
}

/// A count and the kilobytes behind it.
#[derive(Debug, Default)]
struct Tally {
    n: AtomicU64,
    kb: AtomicU64,
}

impl Tally {
    fn add(&self, kb: u64) {
        self.n.fetch_add(1, Relaxed);
        self.kb.fetch_add(kb, Relaxed);
    }

    fn take(&self) -> (u64, u64) {
        (self.n.swap(0, Relaxed), self.kb.swap(0, Relaxed))
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Relaxed);
}

/// The always-on tier: every field is one relaxed atomic.
///
/// Written by the drain thread alone (`note_gather` and the cache's eviction both
/// run there) and swapped to zero by it once a second, so no line is ever shared
/// between cores and the additions never contend. The `Relaxed` ordering is the
/// whole synchronization: these are tallies read for a report, and a bind that
/// lands either side of the swap is simply counted in the neighbouring second.
#[derive(Debug, Default)]
struct Totals {
    binds: AtomicU64,
    vouched: AtomicU64,
    refused: AtomicU64,
    rearmed: AtomicU64,
    rearm_new: AtomicU64,
    rearm_pages: AtomicU64,
    rearm_span: AtomicU64,
    rearm_interrupted: AtomicU64,
    unarmed: Tally,
    /// `[untracked, arming, no_baseline]`.
    unarmed_cause: [AtomicU64; 3],
    unarmed_first: AtomicU64,
    unarmed_repeat: Tally,
    unarmed_same_tranche: AtomicU64,
    unarmed_max_run: AtomicU64,
    unvouched_kb: AtomicU64,
    /// Binds that read no generation, whatever their verdict.
    unreadable: Tally,
    /// Unarmed binds by shadow fold: `[seeded, same, moved, indebted]`.
    fold: [Tally; 4],
    ready_at_birth: AtomicU64,
    ready_late: AtomicU64,
    ready_binds_sum: AtomicU64,
    ready_binds_max: AtomicU64,
    /// `[1, 2..=4, 5..=16, 17..]`.
    ready_binds_band: [AtomicU64; 4],
    ready_tranches_max: AtomicU64,
    /// `[0, 1, 2..=3, 4..]`.
    ready_tranches_band: [AtomicU64; 4],
    ready_us_sum: AtomicU64,
    ready_us_max: AtomicU64,
    /// `[superseded][hit]`: an entry a newer generation of its window had
    /// replaced can never be named again whatever else is true of it, and one
    /// never hit is one the cache held for nothing. The cell that cost a gather is
    /// `[false][true]`: current, and useful.
    evicted: [[Tally; 2]; 2],
    /// Evictions of entries whose window the opt-in table did not see this second
    /// (or that were evicted with the table off).
    evicted_unbound: AtomicU64,
}

/// A second's totals, read out and cleared.
#[derive(Debug, Default)]
struct Snapshot {
    binds: u64,
    vouched: u64,
    refused: u64,
    rearmed: u64,
    rearm_new: u64,
    rearm_pages: u64,
    rearm_span: u64,
    rearm_interrupted: u64,
    unarmed: (u64, u64),
    unarmed_cause: [u64; 3],
    unarmed_first: u64,
    unarmed_repeat: (u64, u64),
    unarmed_same_tranche: u64,
    unarmed_max_run: u64,
    unvouched_kb: u64,
    unreadable: (u64, u64),
    fold: [(u64, u64); 4],
    ready_at_birth: u64,
    ready_late: u64,
    ready_binds_sum: u64,
    ready_binds_max: u64,
    ready_binds_band: [u64; 4],
    ready_tranches_max: u64,
    ready_tranches_band: [u64; 4],
    ready_us_sum: u64,
    ready_us_max: u64,
    evicted: [[(u64, u64); 2]; 2],
    evicted_unbound: u64,
}

impl Totals {
    fn note_bind(&self, span: u64, seen: &GatherObservation) {
        let life = seen.detail.life;
        let kb = kb(span);
        bump(&self.binds);
        match seen.verdict {
            GatherVerdict::Vouched => bump(&self.vouched),
            GatherVerdict::Refused { .. } => {
                bump(&self.refused);
                self.unvouched_kb.fetch_add(kb, Relaxed);
            }
            GatherVerdict::Rearmed => {
                bump(&self.rearmed);
                self.unvouched_kb.fetch_add(kb, Relaxed);
                if let Some(rearm) = seen.detail.rearm {
                    bump(match rearm.why {
                        RearmWhy::New => &self.rearm_new,
                        RearmWhy::PagesMoved => &self.rearm_pages,
                        RearmWhy::SpanMoved => &self.rearm_span,
                    });
                    if rearm.interrupted_arming {
                        bump(&self.rearm_interrupted);
                    }
                }
            }
            GatherVerdict::Unarmed => {
                self.unarmed.add(kb);
                self.unvouched_kb.fetch_add(kb, Relaxed);
                if life.unarmed_run_before == 0 {
                    bump(&self.unarmed_first);
                } else {
                    self.unarmed_repeat.add(kb);
                }
                if life.same_tranche_as_previous {
                    bump(&self.unarmed_same_tranche);
                }
                self.unarmed_max_run
                    .fetch_max(u64::from(life.unarmed_run_before) + 1, Relaxed);
                bump(&self.unarmed_cause[cause_index(seen.detail.unarmed)]);
                match seen.detail.shadow {
                    ShadowFold::Off => {}
                    ShadowFold::Seeded => self.fold[0].add(kb),
                    ShadowFold::Same => self.fold[1].add(kb),
                    ShadowFold::Moved => self.fold[2].add(kb),
                    ShadowFold::Indebted => self.fold[3].add(kb),
                }
            }
        }
        if !life.readable {
            self.unreadable.add(kb);
        }
        if life.first_readable {
            self.note_ready(life.binds, life.tranches, life.us);
        }
    }

    fn note_ready(&self, binds: u32, tranches: u64, us: u64) {
        if binds <= 1 && tranches == 0 {
            bump(&self.ready_at_birth);
            return;
        }
        bump(&self.ready_late);
        let binds = u64::from(binds);
        self.ready_binds_sum.fetch_add(binds, Relaxed);
        self.ready_binds_max.fetch_max(binds, Relaxed);
        bump(
            &self.ready_binds_band[match binds {
                0..=1 => 0,
                2..=4 => 1,
                5..=16 => 2,
                _ => 3,
            }],
        );
        self.ready_tranches_max.fetch_max(tranches, Relaxed);
        bump(
            &self.ready_tranches_band[match tranches {
                0 => 0,
                1 => 1,
                2..=3 => 2,
                _ => 3,
            }],
        );
        self.ready_us_sum.fetch_add(us, Relaxed);
        self.ready_us_max.fetch_max(us, Relaxed);
    }

    fn note_image_evicted(&self, image: EvictedImage) {
        self.evicted[usize::from(image.superseded)][usize::from(image.hits > 0)]
            .add(kb(image.bytes));
    }

    fn take(&self) -> Snapshot {
        let swap = |a: &AtomicU64| a.swap(0, Relaxed);
        let band = |bands: &[AtomicU64; 4]| [0, 1, 2, 3].map(|i| swap(&bands[i]));
        Snapshot {
            binds: swap(&self.binds),
            vouched: swap(&self.vouched),
            refused: swap(&self.refused),
            rearmed: swap(&self.rearmed),
            rearm_new: swap(&self.rearm_new),
            rearm_pages: swap(&self.rearm_pages),
            rearm_span: swap(&self.rearm_span),
            rearm_interrupted: swap(&self.rearm_interrupted),
            unarmed: self.unarmed.take(),
            unarmed_cause: [0, 1, 2].map(|i| swap(&self.unarmed_cause[i])),
            unarmed_first: swap(&self.unarmed_first),
            unarmed_repeat: self.unarmed_repeat.take(),
            unarmed_same_tranche: swap(&self.unarmed_same_tranche),
            unarmed_max_run: swap(&self.unarmed_max_run),
            unvouched_kb: swap(&self.unvouched_kb),
            unreadable: self.unreadable.take(),
            fold: [0, 1, 2, 3].map(|i| self.fold[i].take()),
            ready_at_birth: swap(&self.ready_at_birth),
            ready_late: swap(&self.ready_late),
            ready_binds_sum: swap(&self.ready_binds_sum),
            ready_binds_max: swap(&self.ready_binds_max),
            ready_binds_band: band(&self.ready_binds_band),
            ready_tranches_max: swap(&self.ready_tranches_max),
            ready_tranches_band: band(&self.ready_tranches_band),
            ready_us_sum: swap(&self.ready_us_sum),
            ready_us_max: swap(&self.ready_us_max),
            evicted: [0, 1].map(|s| [0, 1].map(|h| self.evicted[s][h].take())),
            evicted_unbound: swap(&self.evicted_unbound),
        }
    }
}

fn cause_index(cause: Option<UnarmedCause>) -> usize {
    match cause {
        Some(UnarmedCause::Untracked) => 0,
        Some(UnarmedCause::Arming) => 1,
        Some(UnarmedCause::NoBaseline) | None => 2,
    }
}

impl Snapshot {
    fn evicted_total(&self) -> (u64, u64) {
        self.evicted
            .iter()
            .flatten()
            .fold((0, 0), |(n, kb), cell| (n + cell.0, kb + cell.1))
    }

    fn totals_line(&self, win_ms: u64) -> String {
        let [untracked, arming, no_baseline] = self.unarmed_cause;
        let fold = if self.fold.iter().all(|t| t.0 == 0) {
            "fold=off".to_owned()
        } else {
            let [seeded, same, moved, indebted] = self.fold;
            format!(
                "fold_seeded={} fold_same={} fold_same_kb={} fold_moved={} fold_moved_kb={} \
                 fold_indebted={}",
                seeded.0, same.0, same.1, moved.0, moved.1, indebted.0
            )
        };
        format!(
            "gather_storm win_ms={win_ms} binds={} vouched={} refused={} rearmed={} \
             rearm_new={} rearm_pages={} rearm_span={} rearm_interrupted_arming={} \
             unarmed={} unarmed_untracked={untracked} unarmed_arming={arming} \
             unarmed_no_baseline={no_baseline} unarmed_first={} unarmed_repeat={} \
             unarmed_same_tranche={} unarmed_max_run={} unarmed_kb={} unarmed_repeat_kb={} \
             unvouched_kb={} unreadable={} unreadable_kb={} {fold}",
            self.binds,
            self.vouched,
            self.refused,
            self.rearmed,
            self.rearm_new,
            self.rearm_pages,
            self.rearm_span,
            self.rearm_interrupted,
            self.unarmed.0,
            self.unarmed_first,
            self.unarmed_repeat.0,
            self.unarmed_same_tranche,
            self.unarmed_max_run,
            self.unarmed.1,
            self.unarmed_repeat.1,
            self.unvouched_kb,
            self.unreadable.0,
            self.unreadable.1,
        )
    }

    fn ready_line(&self, win_ms: u64) -> String {
        let [b1, b2, b3, b4] = self.ready_binds_band;
        let [t0, t1, t2, t3] = self.ready_tranches_band;
        format!(
            "gather_storm_ready win_ms={win_ms} at_birth={} late={} binds_sum={} binds_max={} \
             binds_1={b1} binds_2_4={b2} binds_5_16={b3} binds_17p={b4} tranches_max={} \
             tranches_0={t0} tranches_1={t1} tranches_2_3={t2} tranches_4p={t3} us_sum={} \
             us_max={}",
            self.ready_at_birth,
            self.ready_late,
            self.ready_binds_sum,
            self.ready_binds_max,
            self.ready_tranches_max,
            self.ready_us_sum,
            self.ready_us_max,
        )
    }

    fn evict_line(&self, win_ms: u64) -> String {
        let e = &self.evicted;
        let (n, kb) = self.evicted_total();
        format!(
            "gather_storm_evict win_ms={win_ms} n={n} kb={kb} superseded_never_hit={} \
             superseded_hit={} current_never_hit={} current_hit={} current_hit_kb={} \
             unbound_windows={}",
            e[1][0].0, e[1][1].0, e[0][0].0, e[0][1].0, e[0][1].1, self.evicted_unbound,
        )
    }
}

/// One cache entry's departure, as the engine reports it.
#[derive(Clone, Copy, Debug)]
pub struct EvictedImage {
    /// `GatheredIdentity::key` of the entry's identity.
    pub content_key: u64,
    pub bytes: u64,
    /// Lookups this entry answered while resident.
    pub hits: u32,
    /// A newer generation of the same window was admitted after this one.
    pub superseded: bool,
}

/// One window's second, in the opt-in table.
#[derive(Clone, Debug)]
struct Row {
    key: GatherKey,
    rail: GatherRail,
    span: u64,
    binds: u64,
    vouched: u64,
    refused: u64,
    rearmed: u64,
    /// `[untracked, arming, no_baseline]`.
    unarmed: [u64; 3],
    /// Kilobytes of this window's binds the witness did not vouch for.
    unvouched_kb: u64,
    max_unarmed_run: u32,
    /// Distinct tranches this window was bound in.
    tranches: u64,
    last_tranche: u64,
    /// The last bind this second read a generation.
    last_readable: bool,
    witness_evicted: u64,
    image_evicted: u64,
    image_evicted_never_hit: u64,
}

impl Row {
    fn new(key: GatherKey, rail: GatherRail, span: u64) -> Self {
        Self {
            key,
            rail,
            span,
            binds: 0,
            vouched: 0,
            refused: 0,
            rearmed: 0,
            unarmed: [0; 3],
            unvouched_kb: 0,
            max_unarmed_run: 0,
            tranches: 0,
            last_tranche: u64::MAX,
            last_readable: false,
            witness_evicted: 0,
            image_evicted: 0,
            image_evicted_never_hit: 0,
        }
    }

    fn apply(&mut self, rail: GatherRail, span: u64, seen: &GatherObservation) {
        let life: EntryLife = seen.detail.life;
        self.binds += 1;
        self.span = span;
        self.rail = rail;
        self.last_readable = life.readable;
        if self.last_tranche != life.tranche {
            self.last_tranche = life.tranche;
            self.tranches += 1;
        }
        match seen.verdict {
            GatherVerdict::Vouched => self.vouched += 1,
            GatherVerdict::Refused { .. } => {
                self.refused += 1;
                self.unvouched_kb += kb(span);
            }
            GatherVerdict::Rearmed => {
                self.rearmed += 1;
                self.unvouched_kb += kb(span);
            }
            GatherVerdict::Unarmed => {
                self.unarmed[cause_index(seen.detail.unarmed)] += 1;
                self.unvouched_kb += kb(span);
                self.max_unarmed_run = self
                    .max_unarmed_run
                    .max(life.unarmed_run_before.saturating_add(1));
            }
        }
    }
}

/// The opt-in tier: one row per window bound this second.
#[derive(Debug, Default)]
struct Rows {
    rows: HashMap<u64, Row>,
    dropped: u64,
}

impl Rows {
    fn note_bind(&mut self, key: GatherKey, rail: GatherRail, span: u64, seen: &GatherObservation) {
        // `content_key` is what the engine names an image by, so it is the row's
        // name and an eviction can find the window it belongs to.
        let content = key.content_key();
        if self.rows.len() >= ROWS_MAX && !self.rows.contains_key(&content) {
            self.dropped += 1;
            return;
        }
        self.rows
            .entry(content)
            .or_insert_with(|| Row::new(key, rail, span))
            .apply(rail, span, seen);
    }

    fn keys_line(&self, win_ms: u64, unvouched_kb: u64) -> Option<String> {
        if self.rows.is_empty() {
            return None;
        }
        let mut rows: Vec<&Row> = self.rows.values().collect();
        rows.sort_by_key(|row| (std::cmp::Reverse(row.unvouched_kb), row.key));
        let shown = rows.len().min(KEYS_REPORTED);
        let top: u64 = rows.iter().take(shown).map(|row| row.unvouched_kb).sum();
        let share = (top * 100).checked_div(unvouched_kb).unwrap_or(0);
        let unready = rows.iter().filter(|row| !row.last_readable).count();
        let mut out = format!(
            "gather_storm_keys win_ms={win_ms} windows={} rows_dropped={} unready={unready} \
             shown={shown} top_share_pct={share}",
            rows.len(),
            self.dropped,
        );
        for row in rows.iter().take(shown) {
            out.push_str(&format!(
                " [{} rail={} span_kb={} binds={} vouched={} refused={} rearmed={} \
                 unarmed={}(untracked={}/arming={}/no_baseline={}) unvouched_kb={} \
                 max_unarmed_run={} tranches={} ready={} witness_evicted={} \
                 image_evicted={}/{}never_hit]",
                row.key.log_token(),
                row.rail.label(),
                kb(row.span),
                row.binds,
                row.vouched,
                row.refused,
                row.rearmed,
                row.unarmed.iter().sum::<u64>(),
                row.unarmed[0],
                row.unarmed[1],
                row.unarmed[2],
                row.unvouched_kb,
                row.max_unarmed_run,
                row.tranches,
                u8::from(row.last_readable),
                row.witness_evicted,
                row.image_evicted,
                row.image_evicted_never_hit,
            ));
        }
        Some(out)
    }
}

/// Both tiers. `rows` is `None` unless the table was asked for.
#[derive(Debug)]
struct Census {
    totals: Totals,
    rows: Option<Mutex<Rows>>,
}

impl Census {
    fn new(table: bool) -> Self {
        Self {
            totals: Totals::default(),
            rows: table.then(Mutex::default),
        }
    }

    fn note_bind(&self, key: GatherKey, rail: GatherRail, span: u64, seen: &GatherObservation) {
        self.totals.note_bind(span, seen);
        if let Some(rows) = &self.rows {
            rows.lock().note_bind(key, rail, span, seen);
        }
    }

    fn note_window_evicted(&self, key: GatherKey) {
        if let Some(rows) = &self.rows {
            if let Some(row) = rows.lock().rows.get_mut(&key.content_key()) {
                row.witness_evicted += 1;
            }
        }
    }

    fn note_image_evicted(&self, image: EvictedImage) {
        self.totals.note_image_evicted(image);
        let attributed = self.rows.as_ref().is_some_and(|rows| {
            rows.lock()
                .rows
                .get_mut(&image.content_key)
                .map(|row| {
                    row.image_evicted += 1;
                    if image.hits == 0 {
                        row.image_evicted_never_hit += 1;
                    }
                })
                .is_some()
        });
        if !attributed {
            bump(&self.totals.evicted_unbound);
        }
    }

    /// The second's lines, clearing both tiers. Empty when nothing bound or was
    /// evicted.
    fn take_lines(&self, win_ms: u64) -> Vec<String> {
        let snap = self.totals.take();
        let rows = self
            .rows
            .as_ref()
            .map(|rows| std::mem::take(&mut *rows.lock()));
        let evicted = snap.evicted_total().0;
        let mut lines = Vec::with_capacity(4);
        if snap.binds != 0 {
            lines.push(snap.totals_line(win_ms));
            lines.push(snap.ready_line(win_ms));
        }
        if evicted != 0 {
            lines.push(snap.evict_line(win_ms));
        }
        if let Some(keys) = rows
            .as_ref()
            .and_then(|rows| rows.keys_line(win_ms, snap.unvouched_kb))
        {
            lines.push(keys);
        }
        lines
    }
}

static CENSUS: std::sync::LazyLock<Census> = std::sync::LazyLock::new(|| {
    Census::new(matches!(
        crate::config::switch(crate::config::GATHER_STORM_KEYS),
        crate::config::Switch::On
    ))
});

/// File one witnessed bind. Called once per `note_gather`.
pub(super) fn note_bind(key: GatherKey, rail: GatherRail, span: u64, seen: &GatherObservation) {
    CENSUS.note_bind(key, rail, span, seen);
}

/// The witness dropped `key`'s entry (and token) to stay under its table bound.
pub(super) fn note_window_evicted(key: GatherKey) {
    CENSUS.note_window_evicted(key);
}

/// The sampled cache dropped a gathered image.
pub fn note_image_evicted(image: EvictedImage) {
    CENSUS.note_image_evicted(image);
}

/// The second's lines, clearing the census. Empty when nothing bound or evicted.
pub fn take_lines(win_ms: u64) -> Vec<String> {
    CENSUS.take_lines(win_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::gather_witness::{
        BindDetail, ContentAudit, EntryLife, GatherVouch, Rearm, StatedGuestWrite,
    };

    const LINEAR: GatherRail = GatherRail::Linear;
    const SPAN: u64 = 2560 * 1024;

    fn gva(gva: u64) -> GatherKey {
        GatherKey::TaskGva { task_id: 5, gva }
    }

    fn seen(verdict: GatherVerdict, detail: BindDetail) -> GatherObservation {
        GatherObservation {
            verdict,
            audit: ContentAudit::Skipped,
            generation: 1,
            vouch: GatherVouch::Fresh,
            stated: StatedGuestWrite::Unaddressed,
            detail,
        }
    }

    fn life(binds: u32, tranche: u64, unarmed_run_before: u32, readable: bool) -> EntryLife {
        EntryLife {
            tranche,
            binds,
            tranches: tranche.saturating_sub(1),
            us: tranche.saturating_sub(1) * 1000,
            unarmed_run_before,
            same_tranche_as_previous: binds > 1,
            readable,
            first_readable: readable && unarmed_run_before > 0,
        }
    }

    fn born(key: GatherKey, ledger: &Census) {
        let detail = BindDetail {
            rearm: Some(Rearm {
                why: RearmWhy::New,
                interrupted_arming: false,
            }),
            life: life(1, 1, 0, false),
            ..BindDetail::default()
        };
        ledger.note_bind(key, LINEAR, SPAN, &seen(GatherVerdict::Rearmed, detail));
    }

    fn unarmed(key: GatherKey, ledger: &Census, binds: u32, run: u32, cause: UnarmedCause) {
        let readable = cause == UnarmedCause::NoBaseline;
        let detail = BindDetail {
            unarmed: Some(cause),
            life: life(binds, 1, run, readable),
            ..BindDetail::default()
        };
        ledger.note_bind(key, LINEAR, SPAN, &seen(GatherVerdict::Unarmed, detail));
    }

    fn field(line: &str, name: &str) -> u64 {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} is not on: {line}"))
            .parse()
            .unwrap_or_else(|_| panic!("{name} is not a number in: {line}"))
    }

    /// The second this was built for, in miniature: a window born unarmed, bound
    /// ten more times while its token is still arming, then once more after it
    /// arms. The line must say the repeats were repeats of one identical window,
    /// that none of them read a generation, and how long the window took to warm up.
    #[test]
    fn a_storm_of_binds_on_an_arming_window_reads_as_repeats_of_one_window() {
        let ledger = Census::new(true);
        born(gva(0x1000), &ledger);
        for run in 0..9 {
            unarmed(gva(0x1000), &ledger, 2 + run, run, UnarmedCause::Arming);
        }
        unarmed(gva(0x1000), &ledger, 11, 9, UnarmedCause::NoBaseline);

        let lines = ledger.take_lines(1000);
        let totals = &lines[0];
        assert!(totals.starts_with("gather_storm win_ms=1000 "), "{totals}");
        assert_eq!(field(totals, "binds"), 11);
        assert_eq!(field(totals, "rearmed"), 1);
        assert_eq!(field(totals, "rearm_new"), 1);
        assert_eq!(field(totals, "unarmed"), 10);
        assert_eq!(field(totals, "unarmed_arming"), 9);
        assert_eq!(field(totals, "unarmed_no_baseline"), 1);
        assert_eq!(field(totals, "unarmed_untracked"), 0);
        assert_eq!(
            field(totals, "unarmed_first"),
            1,
            "the first is not a repeat"
        );
        assert_eq!(field(totals, "unarmed_repeat"), 9);
        assert_eq!(field(totals, "unarmed_max_run"), 10);
        assert_eq!(field(totals, "unarmed_kb"), 10 * 2560);
        assert_eq!(field(totals, "unarmed_repeat_kb"), 9 * 2560);
        assert_eq!(field(totals, "unvouched_kb"), 11 * 2560);
        assert_eq!(
            field(totals, "unreadable"),
            10,
            "the birth and nine arming binds read no generation; the baseline bind did"
        );
        assert!(totals.ends_with("fold=off"), "{totals}");
        // The partition the line promises.
        assert_eq!(
            field(totals, "binds"),
            field(totals, "vouched")
                + field(totals, "refused")
                + field(totals, "rearmed")
                + field(totals, "unarmed")
        );
        assert_eq!(
            field(totals, "unarmed"),
            field(totals, "unarmed_untracked")
                + field(totals, "unarmed_arming")
                + field(totals, "unarmed_no_baseline")
        );

        let ready = &lines[1];
        assert!(ready.starts_with("gather_storm_ready "), "{ready}");
        assert_eq!(field(ready, "late"), 1);
        assert_eq!(field(ready, "at_birth"), 0);
        assert_eq!(field(ready, "binds_max"), 11);
        assert_eq!(field(ready, "binds_5_16"), 1);
    }

    /// A window that never read a generation is the one a reader goes looking
    /// for, and it is named as still unready rather than averaged away.
    #[test]
    fn a_window_that_never_warmed_up_is_counted_as_still_unready() {
        let ledger = Census::new(true);
        born(gva(0x1000), &ledger);
        for run in 0..3 {
            unarmed(gva(0x1000), &ledger, 2 + run, run, UnarmedCause::Untracked);
        }
        let lines = ledger.take_lines(1000);
        assert_eq!(field(&lines[0], "unarmed_untracked"), 3);
        assert_eq!(field(&lines[1], "late"), 0);
        assert_eq!(field(lines.last().expect("a keys line"), "unready"), 1);
    }

    /// The heaviest windows lead, by kilobytes the witness did not vouch for, and
    /// the line says how much of the second they carry.
    #[test]
    fn the_keys_line_leads_with_the_window_that_cost_the_most() {
        let ledger = Census::new(true);
        born(gva(0x1000), &ledger);
        born(gva(0x2000), &ledger);
        for run in 0..5 {
            unarmed(gva(0x2000), &ledger, 2 + run, run, UnarmedCause::Arming);
        }
        let lines = ledger.take_lines(1000);
        let keys = lines.last().expect("a keys line");
        assert!(keys.starts_with("gather_storm_keys "), "{keys}");
        let first = keys
            .find("[gva:5:0x2000")
            .expect("the heavy window is named");
        let second = keys
            .find("[gva:5:0x1000")
            .expect("the light window is named too");
        assert!(first < second, "{keys}");
        assert_eq!(field(keys, "windows"), 2);
        assert_eq!(field(keys, "top_share_pct"), 100);
        assert!(
            keys.contains("unarmed=5(untracked=0/arming=5/no_baseline=0)"),
            "{keys}"
        );
        assert!(keys.contains("max_unarmed_run=5"), "{keys}");
        assert!(keys.contains("rail=linear"), "{keys}");
    }

    /// One window bound in three tranches counts three, and one bound three
    /// times in one tranche counts one: the line's way of telling a burst inside
    /// one drain from a window that stays unready across several.
    #[test]
    fn a_window_counts_the_tranches_it_was_bound_in() {
        let ledger = Census::new(true);
        for (binds, tranche) in [(1u32, 4u64), (2, 4), (3, 5), (4, 5), (5, 9)] {
            let detail = BindDetail {
                unarmed: Some(UnarmedCause::Arming),
                life: life(binds, tranche, binds - 1, false),
                ..BindDetail::default()
            };
            ledger.note_bind(
                gva(0x1000),
                LINEAR,
                SPAN,
                &seen(GatherVerdict::Unarmed, detail),
            );
        }
        let keys = ledger.take_lines(1000).pop().expect("a keys line");
        assert!(keys.contains("tranches=3"), "{keys}");
    }

    /// Past the table bound a bind still reaches every total and the loss is
    /// reported, because a bound that silently thinned the totals would make the
    /// storm read smaller exactly when it is largest.
    #[test]
    fn a_full_table_drops_rows_and_not_totals() {
        let ledger = Census::new(true);
        for i in 0..(ROWS_MAX as u64 + 3) {
            born(gva(0x1000 * (i + 1)), &ledger);
        }
        let lines = ledger.take_lines(1000);
        assert_eq!(field(&lines[0], "binds"), ROWS_MAX as u64 + 3);
        let keys = lines.last().expect("a keys line");
        assert_eq!(field(keys, "windows"), ROWS_MAX as u64);
        assert_eq!(field(keys, "rows_dropped"), 3);
    }

    /// With the table off — the shipping arm — no row is ever built, the keys line
    /// never appears, and every total reads exactly what it reads with it on.
    /// The table is a view over the same binds, not a second source of them.
    #[test]
    fn the_totals_do_not_depend_on_the_table() {
        let feed = |census: &Census| {
            born(gva(0x1000), census);
            for run in 0..9 {
                unarmed(gva(0x1000), census, 2 + run, run, UnarmedCause::Arming);
            }
            unarmed(gva(0x1000), census, 11, 9, UnarmedCause::NoBaseline);
            census.note_image_evicted(EvictedImage {
                content_key: gva(0x1000).content_key(),
                bytes: 2560 * 1024,
                hits: 0,
                superseded: true,
            });
        };
        let off = Census::new(false);
        let on = Census::new(true);
        feed(&off);
        feed(&on);
        assert!(off.rows.is_none(), "no table is built while it is off");

        let off_lines = off.take_lines(1000);
        let on_lines = on.take_lines(1000);
        assert!(off_lines
            .iter()
            .all(|l| !l.starts_with("gather_storm_keys")));
        assert!(on_lines.iter().any(|l| l.starts_with("gather_storm_keys")));
        let totals = |lines: &[String]| {
            lines
                .iter()
                .filter(|l| !l.starts_with("gather_storm_keys"))
                .map(|l| {
                    // The one column whose meaning depends on the table: with it
                    // off, no eviction can be attributed to a window.
                    l.split_whitespace()
                        .filter(|part| !part.starts_with("unbound_windows="))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(totals(&off_lines), totals(&on_lines));
        let unbound = |lines: &[String]| {
            field(
                lines
                    .iter()
                    .find(|l| l.starts_with("gather_storm_evict "))
                    .expect("an evict line"),
                "unbound_windows",
            )
        };
        assert_eq!(unbound(&off_lines), 1, "off: nothing to attribute it to");
        assert_eq!(unbound(&on_lines), 0, "on: the window bound this second");
    }

    /// What the two tiers cost per bind, on the host that runs it.
    ///
    /// Not an assertion: the figures are the reading and they belong to the
    /// machine. Run with
    /// `cargo test --release -p reims-vgpu --lib -- --ignored --nocapture census_cost`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn census_cost() {
        const BINDS: u64 = 2_000_000;
        let bind = |n: u64| {
            let run = (n % 12) as u32;
            let detail = BindDetail {
                unarmed: Some(UnarmedCause::Arming),
                life: life(run + 2, 1 + n / 600, run, false),
                ..BindDetail::default()
            };
            seen(GatherVerdict::Unarmed, detail)
        };
        let observations: Vec<GatherObservation> = (0..64).map(bind).collect();
        let keys: Vec<GatherKey> = (0..57).map(|i| gva(0x1000 * (i + 1))).collect();
        let time = |census: &Census| {
            let started = std::time::Instant::now();
            for n in 0..BINDS {
                census.note_bind(
                    keys[(n % 57) as usize],
                    LINEAR,
                    SPAN,
                    &observations[(n % 64) as usize],
                );
                if n % 1_000_000 == 999_999 {
                    let _ = census.take_lines(1000);
                }
            }
            started.elapsed().as_nanos() as f64 / BINDS as f64
        };
        let clock = {
            let started = std::time::Instant::now();
            let mut sink = 0u64;
            for _ in 0..BINDS {
                sink = sink.wrapping_add(crate::observe::elapsed_us());
            }
            std::hint::black_box(sink);
            started.elapsed().as_nanos() as f64 / BINDS as f64
        };
        let (off, on) = (time(&Census::new(false)), time(&Census::new(true)));
        eprintln!(
            "census_cost ns/bind: totals only {off:.1}, with the table {on:.1}, \
             one elapsed_us() read {clock:.1}"
        );
    }

    /// An eviction is sorted by the two facts that say whether it cost a gather,
    /// and attributed to the window that lost the image when that window bound
    /// this second.
    #[test]
    fn an_eviction_says_whether_the_image_could_still_have_been_found() {
        let ledger = Census::new(true);
        born(gva(0x1000), &ledger);
        let content_key = gva(0x1000).content_key();
        let evict = |ledger: &Census, hits, superseded, content_key| {
            ledger.note_image_evicted(EvictedImage {
                content_key,
                bytes: 2560 * 1024,
                hits,
                superseded,
            });
        };
        evict(&ledger, 0, true, content_key);
        evict(&ledger, 0, true, content_key);
        evict(&ledger, 3, false, content_key);
        evict(&ledger, 0, false, 0xdead);

        let lines = ledger.take_lines(1000);
        let evicted = lines
            .iter()
            .find(|l| l.starts_with("gather_storm_evict "))
            .expect("an evict line");
        assert_eq!(field(evicted, "n"), 4);
        assert_eq!(field(evicted, "superseded_never_hit"), 2);
        assert_eq!(field(evicted, "current_hit"), 1);
        assert_eq!(field(evicted, "current_hit_kb"), 2560);
        assert_eq!(field(evicted, "current_never_hit"), 1);
        assert_eq!(field(evicted, "unbound_windows"), 1);
        let keys = lines.last().expect("a keys line");
        assert!(keys.contains("image_evicted=3/2never_hit"), "{keys}");
    }

    /// Taking clears, and a second with nothing in it writes nothing.
    #[test]
    fn a_quiet_second_writes_no_line() {
        let ledger = Census::new(true);
        assert!(ledger.take_lines(1000).is_empty());
        born(gva(0x1000), &ledger);
        assert!(!ledger.take_lines(1000).is_empty());
        assert!(ledger.take_lines(1000).is_empty());
    }

    /// The fold's verdict divides the unarmed binds and only appears when it ran.
    #[test]
    fn the_shadow_fold_columns_appear_only_when_a_fold_ran() {
        let ledger = Census::new(true);
        for (n, shadow) in [
            ShadowFold::Seeded,
            ShadowFold::Same,
            ShadowFold::Same,
            ShadowFold::Moved,
        ]
        .into_iter()
        .enumerate()
        {
            let detail = BindDetail {
                unarmed: Some(UnarmedCause::Arming),
                life: life(n as u32 + 1, 1, n as u32, false),
                shadow,
                ..BindDetail::default()
            };
            ledger.note_bind(
                gva(0x1000),
                LINEAR,
                SPAN,
                &seen(GatherVerdict::Unarmed, detail),
            );
        }
        let totals = ledger.take_lines(1000).remove(0);
        assert_eq!(field(&totals, "fold_seeded"), 1);
        assert_eq!(field(&totals, "fold_same"), 2);
        assert_eq!(field(&totals, "fold_same_kb"), 2 * 2560);
        assert_eq!(field(&totals, "fold_moved"), 1);
        assert!(!totals.contains("fold=off"), "{totals}");
    }
}
