//! Cache-hit map: a second independent window, showing **how much of this conversation's context
//! the provider served from its prompt cache**.
//!
//! Same shape as `activity.rs` (own viewport, shared state, self-drawn top bar), with the window
//! furniture itself now living in `gwin.rs` — see that module for the drag / resize / corners /
//! poker story.
//!
//! # One clock: the front paints, and what it paints
//!
//! The 16×16 grid is a **time strip**, not a history of calls. A front walks the square along a
//! Hilbert curve (see [`WALK`]) at one tile every 98 ms — 256 tiles in 25 seconds — and at each step
//! it writes **the newest reading there is** into the next tile: the provider's own `prompt` /
//! `cached` figures for the most recent call. When no new reading has arrived it writes the same one
//! again, which is why a reading that held for three seconds leaves a band about thirty tiles wide.
//!
//! The colour is a band on the one ratio that *is* measured, `cached / prompt`:
//!
//! - **Hit** — the cache served (nearly) all of the prompt.
//! - **Partial** — it served a large part: the prefix that was already on the server came back, the
//!   tail that had just been appended did not.
//! - **Miss** — (nearly) nothing was reused: a cold start, or a cache that had expired.
//!
//! Those two edges are the only invented thing here, which is why the tooltip always prints the raw
//! ratio and the token counts beside the label — the band can be argued with, the numbers cannot.
//!
//! There is one writer and one clock, and that is the correction this revision records. The earlier
//! revisions had two: a ring of the last 256 calls (written by calls, faded by call age) plus a scan
//! that revealed that ring in walk order. The two axes disagreed — the front spent its time
//! repainting *old* calls while the newest reading sat in the middle of the grid wearing the frame —
//! so what the eye was shown was a replay, not a reading. Now the front paints the newest thing it
//! knows, and brightness is nothing but its own distance behind the front.
//!
//! A lap erases nothing. The first pass fills the square — a blank tile is one the front has never
//! reached — and from then on it is always full, the next pass overwriting the tile closest to being
//! due. So the picture is a rolling 25 seconds.
//!
//! **The clock stops when the work stops.** The front only walks while a turn is in flight, because
//! that is the only situation in which another reading is actually coming: a model call in progress
//! is time this reading has genuinely been the newest, so it earns its band. Idle, a question
//! waiting for the user, a plugin waiting for a permission — in those the next reading is blocked on
//! a person, so the picture freezes where it is and the history stays worth reading. (Without this
//! an idle minute scrubbed the square down to one flat colour: honest, and useless.)
//!
//! Each tile is one **band**: near black for a hit, a muted gold for a partial, a soft brick for a
//! miss. The bands are a summary, and it is worth saying what they cannot say: a call that came back
//! 90% cached is a hit here, painted solid, even though a tenth of its prompt was computed again. The
//! tile is the *state*, not the percentage — the percentage is in the tooltip, and in the lifetime
//! figure beside the window.
//!
//! (An earlier attempt split each tile so the bottom `1 - cached/prompt` of it was brick, which does
//! show the tenth. It was rejected: a second scale inside a 20-point square turns the grid into a
//! field of half-filled chips, and at 98% the strip is a one-pixel hairline that reads as nothing.)
//!
//! The older projection of the context's 256 blocks onto the grid stays recorded as a dead end: a
//! prefix cache is a *contiguous* run, so block colouring could only ever draw one boundary and two
//! solid bands — a picture that stated "the answer is a percentage" very loudly and said nothing
//! about the conversation it was supposed to be about.
//!
//! # Why the walk is a Hilbert curve
//!
//! The tiles are laid out along a **Hilbert curve** (see [`WALK`]): consecutive positions are
//! neighbouring cells. So the strip is one unbroken walk over the square — the front always stands
//! next to the tile it is about to repaint, and the first lap grows as a blob out of one corner
//! instead of as rows filling left to right.
//!
//! A row-by-row snake would also keep the line unbroken, but it is really a 1-D strip folded twice:
//! its turns happen only at the edges, so the picture would say nothing about how much of the square
//! is taken until it was almost full. The Hilbert curve turns at every scale.
//!
//! # The frame, and the one thing it marks
//!
//! One frame, on the tile the front has just written, and nothing else: no arrow, no glyph. It marks
//! *now* — it moves one cell every 98 ms and sits on the newest reading by construction. It pulses
//! while a turn is running, the same rule the rest of the window follows: a mark that blinks in an
//! idle window claims something is happening.
//!
//! Frames are asked for only while the front is walking. A stopped window is a still picture, and a
//! self-repaint request would wake the main window every tick to re-lay-out its transcript so that
//! nothing could change.
//!
//! # Where the numbers come from
//!
//! Two messages, at two different moments, because they are two different facts:
//!
//! - **Before each call**, the agent thread reports what it is about to send, split into sections
//!   (system prompt / tools / history / your input) — see [`Self::observe_context`]. That is an
//!   *estimate*, and it is the only thing that exists while the call is in flight.
//! - **After each call**, the provider's own figures arrive per call ([`Self::observe_call`]) and,
//!   once the turn is over, the turn's aggregate ([`Self::observe_turn`]).
//!
//! The grid follows the per-call figures. The aggregate is used for the lifetime totals and as a
//! fallback for endpoints that report nothing per call.

use std::sync::{Arc, Mutex, MutexGuard};

use egui::{
    Align2, Color32, CornerRadius, FontId, Frame, Margin, Order, Pos2, Rect, RichText, Sense,
    Stroke, Ui, Vec2, ViewportId,
};

use aox::history::UsageRecord;
use aox::llm::{thousands, Usage};

use crate::gwin::{self, Gwin};
use crate::theme::{self, P};

/// The grid is 16×16. Not a tunable: the window's whole layout (cell size, the axis labels, the
/// tooltip's token arithmetic) is built around "256 blocks", and a second grid size would be a
/// second layout to maintain for no gain — 256 is already finer than the eye can resolve.
pub const GRID: u32 = 16;
const BLOCKS: u32 = GRID * GRID;

const MIN_W: f32 = 760.0;
const MIN_H: f32 = 580.0;

/// Width of the floating statistics column on the right.
const SIDE_W: f32 = 216.0;
/// Gap between the grid and that column.
const SIDE_GAP: f32 = 16.0;
/// Cell gap inside the grid.
const CELL_GAP: f32 = 3.0;
/// Width of the row-number gutter to the left of the grid.
///
/// Wide enough for a two-digit row number with a comfortable right margin. It used to be wider
/// still: the snake walk printed each row's **travel direction** here (`1 →`, `2 ←`), and a snaked
/// grid is unreadable without it. A Hilbert curve has no per-row direction to print, so the
/// chevron went and the gutter shrank with it.
const GUTTER: f32 = 24.0;

/// The share of the prompt above which a call counts as a **hit**, and below which it counts as a
/// **miss**. Anything in between is a partial.
///
/// These two numbers are the only thing in this window that is a judgement rather than a
/// measurement, so they should be stated plainly. An OpenAI-compatible prefix cache is
/// all-or-nothing *at the prefix*, which in practice means a warm call reports ~95-100% and a cold
/// one reports ~0% — the band in between is where a call lands when it appended a message bigger
/// than the cache could absorb. The edges sit wide apart because that gap is real: a call at 70% is
/// not "nearly a hit", it is a call that just paid for a third of its prompt.
const HIT_MIN: f32 = 0.90;
const MISS_MAX: f32 = 0.10;

/// How much of its strength a tile loses by the time the front comes round to it again.
///
/// The tile just painted is at 1.0 and the one about to be repainted at `1 - FADE_MAX`, linearly by
/// distance behind the front — the reference sheet's 0.7, kept as it was. It never reaches zero: a
/// tile you cannot see is a tile you cannot count, and the fade says "a while ago", not "gone".
const FADE_MAX: f32 = 0.7;

/// Seconds the front takes to cross the whole 256-tile curve once.
///
/// The user's number, given as a speed reference: one full pass fills the square in about 25
/// seconds, then the next pass starts. That works out at 10.2 tiles a second — one tile every 98 ms,
/// which is also the cadence the reference sheet's own demo runs at (90 ms), so a lap takes about as
/// long here as it does there.
///
/// This is now the window's **only** clock. Each step writes the newest reading into the next tile,
/// so a lap is not a replay of anything: it is 25 seconds of readings, sampled every 98 ms, and the
/// picture is always as fresh as the last measurement.
const SWEEP_SECS: f32 = 25.0;

/// Tiles the front crosses per second — `BLOCKS / SWEEP_SECS` ≈ 10.2, one every 98 ms.
///
/// A rate rather than a period because that is the unit `advance` integrates in: the front's
/// position is a walk coordinate in tiles, so the accumulator is compared against `1.0` and the
/// paint step never has to be rescheduled.
const SWEEP_RATE: f32 = BLOCKS as f32 / SWEEP_SECS;

/// Seconds one tile covers — the width of a reading's footprint in the picture.
///
/// Used for the tooltip's age, which is a distance in tiles turned into the time it stands for.
const TILE_SECS: f32 = SWEEP_SECS / BLOCKS as f32;

// ─────────────────────────── The walk order ───────────────────────────

/// The Hilbert curve as a table: `WALK[d]` is the row-major cell index (`row * GRID + col`) that the
/// curve's `d`-th block occupies.
///
/// Built at compile time rather than per frame. It is 256 entries of pure integer arithmetic, so a
/// `const` table costs nothing at runtime and keeps the fill pass a plain array lookup — the
/// alternative, calling the generator 256 times per frame, would be the most expensive thing in the
/// window.
const WALK: [u16; BLOCKS as usize] = walk_table();

/// The inverse table, `POS[row * GRID + col] = d`.
///
/// The hover path finds a cell by its geometry and needs the block number back; searching [`WALK`]
/// for it would be 256 comparisons per frame for a value that never changes.
const POS: [u16; BLOCKS as usize] = inv_table();

/// The `d2xy` step of the Hilbert curve: block `d` → `(row, col)`.
///
/// Index the low bits of `d` from the smallest scale up, mirroring and transposing the quadrant
/// each time. Straight from the standard formulation (the one the little JS this was modelled on
/// also uses), with `(x, y)` swapped on the way out because the algorithm speaks `(column, row)`.
const fn hilbert(d: u32) -> (u32, u32) {
    let mut x = 0u32;
    let mut y = 0u32;
    let mut t = d;
    let mut s = 1u32;
    while s < GRID {
        let rx = (t >> 1) & 1;
        let ry = (t ^ rx) & 1;
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            let tmp = x;
            x = y;
            y = tmp;
        }
        x += s * rx;
        y += s * ry;
        t >>= 2;
        s *= 2;
    }
    (y, x)
}

const fn walk_table() -> [u16; BLOCKS as usize] {
    let mut a = [0u16; BLOCKS as usize];
    let mut d = 0u32;
    while d < BLOCKS {
        let (r, c) = hilbert(d);
        a[d as usize] = (r * GRID + c) as u16;
        d += 1;
    }
    a
}

const fn inv_table() -> [u16; BLOCKS as usize] {
    let mut a = [0u16; BLOCKS as usize];
    let mut d = 0u32;
    while d < BLOCKS {
        let (r, c) = hilbert(d);
        a[(r * GRID + c) as usize] = d as u16;
        d += 1;
    }
    a
}

/// The grid cell block `i` occupies, as `(row, col)`.
///
/// Consecutive blocks are always **neighbouring cells** — the curve never jumps — which is what
/// makes the 256 cells read as one unbroken line rather than as a scatter of positions.
pub fn cell_of(i: u32) -> (u32, u32) {
    let k = u32::from(WALK[(i as usize).min(BLOCKS as usize - 1)]);
    (k / GRID, k % GRID)
}

/// The inverse of [`cell_of`]: which block sits at `(row, col)`.
///
/// Needed by the hover path, which finds a cell by its geometry and has to turn that back into a
/// block number before it can say anything about it.
pub fn flow_of(row: u32, col: u32) -> u32 {
    let k = (row.min(GRID - 1) * GRID + col.min(GRID - 1)) as usize;
    u32::from(POS[k])
}

// ───────────────────────────── The state ─────────────────────────────

/// What one call's cache figures came out as.
///
/// Derived from a single measured ratio — `cached / prompt`, banded at [`HIT_MIN`] and
/// [`MISS_MAX`] — and never from a guess about *which* part of the prompt was reused. See the
/// module docs: the band edges are the only invented thing in this window, so they are also the only
/// thing a reader has to take on trust, and the tooltip always prints the ratio they were applied to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// Essentially the whole prompt came back from the cache.
    Hit,
    /// A large part did: the cached prefix, but not the tail this call appended.
    Partial,
    /// Essentially nothing did: a cold start, or a cache that had expired.
    Miss,
}

impl Verdict {
    /// Band a measured share into one of the three states.
    fn of(share: f32) -> Self {
        if share >= HIT_MIN {
            Verdict::Hit
        } else if share > MISS_MAX {
            Verdict::Partial
        } else {
            Verdict::Miss
        }
    }

    /// Credit towards the headline: a partial is half a hit, a miss is nothing.
    ///
    /// The one place the three states are made comparable, and it is written down rather than
    /// implied: the headline means "the share of these calls' prompts that came back for free".
    fn score(self) -> f32 {
        match self {
            Verdict::Hit => 1.0,
            Verdict::Partial => 0.5,
            Verdict::Miss => 0.0,
        }
    }
}

/// Whether one tile stands for a single model call or for a whole round's total.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    /// A per-call reading: the numbers are that one request.
    Call,
    /// A turn's aggregate: the numbers are the sum over its calls, and the tooltip says how many.
    Round,
}

/// One entry in the strip: a reading exactly as the provider described it.
///
/// The figures are kept **per tile** rather than only in a running total, because the tile *is* the
/// reading: the tooltip can then print this call's own prompt and cached counts instead of an
/// average, and the band the tile was coloured by can be checked against the ratio it came from.
#[derive(Clone, Copy)]
struct Rec {
    verdict: Verdict,
    /// Input tokens this reading sent.
    prompt: u64,
    /// …and how many of them the provider served from its cache.
    cached: u64,
    /// How many model calls this tile stands for — 1 for a live call, more for a round aggregate.
    calls: u32,
    /// Which call (or round) this tile stands for, 1-based, so a tile can be named and not only
    /// pointed at.
    seq: u32,
    unit: Unit,
}

impl Rec {
    /// What an untouched slot holds. Nothing draws it: [`CacheView::painted`] is what says whether a
    /// position has been written, and an unwritten one is left as bare panel rather than painted
    /// faint — see [`CacheView::has`].
    const EMPTY: Rec = Rec {
        verdict: Verdict::Miss,
        prompt: 0,
        cached: 0,
        calls: 0,
        seq: 0,
        unit: Unit::Call,
    };

    /// One finished call.
    fn call(seq: u32, prompt: u64, cached: u64) -> Rec {
        let cached = cached.min(prompt);
        Rec {
            verdict: Verdict::of(share(prompt, cached)),
            prompt,
            cached,
            calls: 1,
            seq,
            unit: Unit::Call,
        }
    }

    /// One turn's aggregate, standing for `calls` model calls.
    ///
    /// Coarser than a call and it says so — the tooltip prints "Round #n · k calls" — because the
    /// ledger of an old conversation keeps sums, and splitting a sum back into per-call figures
    /// would be an invention rather than a reading.
    fn round(seq: u32, calls: u32, prompt: u64, cached: u64) -> Rec {
        let cached = cached.min(prompt);
        Rec {
            verdict: Verdict::of(share(prompt, cached)),
            prompt,
            cached,
            calls: calls.max(1),
            seq,
            unit: Unit::Round,
        }
    }

    /// The share of this reading's prompt that came back from the cache, 0.0..=1.0.
    fn share(&self) -> f32 {
        share(self.prompt, self.cached)
    }
}

/// `cached / prompt`, with the zero case handled once for the three places that need it.
fn share(prompt: u64, cached: u64) -> f32 {
    if prompt == 0 {
        0.0
    } else {
        cached as f32 / prompt as f32
    }
}

/// One section of the assembled request, as reported by the agent thread.
///
/// `label` holds the **English source string** (an i18n key), not the translated text: the window
/// re-translates on every paint, so switching language or theme re-renders it without a restart.
/// It is `&'static str` because that is exactly what `aox::i18n::t` takes, and the section
/// vocabulary is fixed and owned by the host rather than by the agent.
#[derive(Clone)]
pub struct Seg {
    pub label: &'static str,
    pub tokens: u64,
}

pub struct CacheView {
    /// The window shell: open / pinned / busy / spin. See `gwin`.
    pub gw: Gwin,

    /// Current context occupancy in tokens (`UsageRecord::last_prompt`) as the provider measured it,
    /// 0 = nothing measured yet.
    used: u64,
    /// Input tokens the provider served from cache on the last call.
    hit: u64,
    /// Input tokens it did not, on the last call.
    miss: u64,

    /// Size of the request **currently in flight**, summed from the sections the agent reported for
    /// it. Cleared the moment a real measurement for that call lands — this is a stand-in, not a
    /// second source of truth, and it must never outlive the call it describes.
    ///
    /// It exists because the two messages that describe a call are separated by the call itself:
    /// the section breakdown arrives before, the usage figures after. Without this the window would
    /// have nothing to say for the entire duration of a request, which is most of the time it is
    /// worth looking at.
    est: u64,

    /// The strip: `cells[i]` is the last reading the front **painted** at walk position `i` (see
    /// [`cell_of`]), not the reading of call `i`.
    ///
    /// That distinction is the whole model. A cell is a moment, so two cells painted 98 ms apart
    /// while nothing new was measured hold the same reading — a band is a reading that lasted. A
    /// fixed array rather than a growing log because the strip rolls: the front comes round and
    /// overwrites, so the window never needs a scrollbar however long the conversation runs.
    cells: [Rec; BLOCKS as usize],
    /// Walk position the front is about to paint next.
    ///
    /// This is the window's whole notion of "now": the tile just behind it is the newest thing on
    /// screen and wears the frame, and the tile at `front` itself is the one closest to being
    /// repainted.
    front: u32,
    /// How many positions have been painted at least once, saturating at [`BLOCKS`] — the fill state
    /// of the first lap, and the denominator of every percentage in the statistics column.
    ///
    /// Until it reaches [`BLOCKS`] the painted cells are exactly `0..painted`, because the front only
    /// ever moves by one and starts at zero; after that every cell has content for good.
    painted: u32,
    /// Fraction of a tile the front has accumulated towards its next step, `0.0..1.0`.
    sweep: f32,
    /// The newest reading there is — what the front writes. `None` until something has been measured,
    /// which is also what keeps an untouched window from animating at all.
    ///
    /// Held rather than read off a cell, because there is no longer a cell that *is* the newest call:
    /// the reading outlives the tile it was painted on, and the front may be several tiles away from
    /// it (or a whole lap, if the conversation has gone quiet).
    latest: Option<Rec>,
    /// Model calls observed over the whole conversation. Runs past [`BLOCKS`]; it is the numbering
    /// behind "Call #n" in the tooltip, not a position in the picture.
    calls: u32,

    /// Sections of the request, in the order they were assembled.
    segs: Vec<Seg>,

    /// Cumulative hits over the conversation — the "tokens saved" figure.
    saved: u64,
    /// Cumulative input tokens over the conversation (the denominator of the lifetime ratio).
    prompt_total: u64,
    /// Turns observed, for the header's "· N rounds".
    rounds: u32,
    /// Whether any observed call reported cache fields at all. Endpoints differ, and one that never
    /// mentions caching should say so rather than show a confident 0%.
    has_cache: bool,
    /// Set when the last observation carried no usage at all, so the window can say "no data"
    /// instead of printing a 0% it did not measure.
    seen: bool,
    /// Whether a **per-call** figure has already been applied for the request in flight.
    ///
    /// Read by [`Self::observe_turn`] to decide whether the turn's aggregate may still speak for the
    /// reading. It usually may not: `UsageRecord::usage` sums the turn's calls, so its `cached`
    /// counts tokens that are no longer in the last request at all, and letting it overwrite the
    /// context figures would make the window disagree with the request it claims to describe. It
    /// must also not write a *second* tile for work that already has one.
    call_seen: bool,

    /// Per-frame time base from the main window, matching `Activity::now`.
    now: f32,
    /// Phase for the breathing dot and the "currently reading" pulse.
    pulse: f32,
}

impl Default for CacheView {
    fn default() -> Self {
        Self {
            gw: Gwin::default(),
            used: 0,
            hit: 0,
            miss: 0,
            est: 0,
            cells: [Rec::EMPTY; BLOCKS as usize],
            front: 0,
            painted: 0,
            sweep: 0.0,
            latest: None,
            calls: 0,
            segs: Vec::new(),
            saved: 0,
            prompt_total: 0,
            rounds: 0,
            has_cache: false,
            seen: false,
            call_seen: false,
            now: 0.0,
            pulse: 0.0,
        }
    }
}

impl CacheView {
    pub fn is_open(&self) -> bool {
        self.gw.is_open()
    }

    /// Per-frame sync from the main window: the time base and whether a turn is running.
    ///
    /// The arrival flash is deliberately **not** timed from here. `now` is the main window's job
    /// clock, which stops when the program is idle and only advances as often as the other window
    /// repaints — fine for a readout, hopeless for anything that moves. The flash runs off this
    /// window's own frame clock instead; see [`Self::advance`].
    ///
    /// `now` does drive the frame's throb, and that is the right clock for it: a busy window repaints
    /// beside the main one anyway, and a *time* base keeps the blink at the same cadence whatever rate
    /// that turns out to be — a per-frame counter would blink twice as fast on a 120 Hz display.
    pub fn tick(&mut self, now: f32, busy: bool) {
        self.now = now;
        // TEMP-PROBE
        self.gw.busy = busy || std::env::var_os("AOX_KEEP_WALKING").is_some();
        self.pulse = now;
    }

    /// Move the strip on by one frame — the only clock in the window, and a clock that stops.
    ///
    /// `dt` is the **frame** time of the window that is drawing this, so the strip is as smooth as
    /// that window renders and independent of whether the rest of the program is awake. Clamped
    /// because a frame that arrives after the window was minimised, or after a long stall, must not
    /// teleport the front: 0.1 s is 1.02 tiles, so the loop below runs at most twice per frame and no
    /// tile can be skipped.
    ///
    /// The accumulator is drained one tile at a time rather than pushed through in one go, because
    /// each step writes and stepping twice must write twice — a repeated reading is a band, and a
    /// band is the honest picture of "nothing changed for 200 ms".
    ///
    /// **Nothing in flight, nothing painted.** The front is the thing that writes, and it only has
    /// something to write while the agent is working: a turn in mid-flight will produce another
    /// reading, so time spent waiting on the model is time this reading has really been the newest.
    /// An idle window, a question waiting on the user, a plugin asking for permission — in all three
    /// the next reading is not coming until the user acts, so the picture stops where it is and the
    /// history stays readable. Before this rule, a quiet 25 seconds scrubbed the whole square down to
    /// one flat reading, which is truthful and useless.
    ///
    /// `sweep` keeps its fraction across the stop, so the front resumes mid-tile rather than jumping.
    pub fn advance(&mut self, dt: f32) {
        if !self.gw.busy {
            return;
        }
        self.sweep += dt.clamp(0.0, 0.1) * SWEEP_RATE;
        while self.sweep >= 1.0 {
            self.sweep -= 1.0;
            self.paint();
        }
    }

    /// Write the newest reading into the tile at the front and step on.
    ///
    /// Does nothing at all until something has been measured: an empty strip should stand still
    /// rather than sweep over a square that has nothing to say. It is also what makes an untouched
    /// window cost no frames — see `window()`.
    fn paint(&mut self) {
        let Some(rec) = self.latest else {
            return;
        };
        self.cells[self.front as usize] = rec;
        self.front = (self.front + 1) % BLOCKS;
        self.painted = (self.painted + 1).min(BLOCKS);
    }

    /// Whether the front has ever reached position `i` — i.e. whether there is a tile there.
    ///
    /// Until the first lap is done the painted cells are exactly `0..painted` (the front only moves
    /// one step at a time from zero), and after that the answer is always yes. A blank tile is
    /// therefore not "no data" but "not reached yet", which is why the first 25 seconds look like a
    /// square being drawn rather than an empty one.
    fn has(&self, i: u32) -> bool {
        self.painted >= BLOCKS || i < self.painted
    }

    /// The tile at walk position `i` — `None` if the front has never reached it.
    fn at(&self, i: u32) -> Option<Rec> {
        if self.has(i) {
            Some(self.cells[(i % BLOCKS) as usize])
        } else {
            None
        }
    }

    /// The tile the front has just written — the brightest one, and the only one wearing a frame.
    /// `None` before the first paint.
    fn now_cell(&self) -> Option<u32> {
        if self.painted == 0 {
            None
        } else {
            Some((self.front + BLOCKS - 1) % BLOCKS)
        }
    }

    /// How many tiles back position `i` was painted: 0 for the tile just written, `BLOCKS - 1` for the
    /// one closest to being repainted. Circular, so "old" is a distance rather than a fixed end.
    fn behind(&self, i: u32) -> u32 {
        (self.front + BLOCKS - 1 - i % BLOCKS) % BLOCKS
    }

    /// How strongly a tile is painted: full for the one just written, fading linearly to
    /// `1 - FADE_MAX` at the far end of the walk.
    ///
    /// The fade is the reference sheet's, and it is now the *only* thing that encodes age — the
    /// colour says what was measured, the strength says how long ago. It flattens at 30% instead of
    /// running down to nothing because a tile you cannot see is a tile you cannot count.
    fn fade(&self, i: u32) -> f32 {
        1.0 - FADE_MAX * self.behind(i) as f32 / BLOCKS as f32
    }

    /// How many tiles are hit / partial / miss. Counted off the strip, so the three rows of the
    /// statistics card always add up to exactly what is on screen.
    fn tally(&self) -> (u32, u32, u32) {
        let mut t = (0, 0, 0);
        for i in 0..self.painted {
            match self.cells[i as usize].verdict {
                Verdict::Hit => t.0 += 1,
                Verdict::Partial => t.1 += 1,
                Verdict::Miss => t.2 += 1,
            }
        }
        t
    }

    /// The headline: the picture's own weighted hit rate, `None` while nothing has been painted.
    ///
    /// Deliberately the *picture's* rate and not the context's, so a grid full of grey can never sit
    /// under "97%". It is a share of **time**, not of calls, and that is the right denominator now:
    /// the tiles are seconds, so 62% hit means "the cache was in a hit state for 15 of the last 25
    /// seconds". The context's own share is printed in the statistics column, under "This call",
    /// where it is labelled as the different question it is.
    fn map_ratio(&self) -> Option<f32> {
        if self.painted == 0 {
            return None;
        }
        let score: f32 = (0..self.painted).map(|i| self.cells[i as usize].verdict.score()).sum();
        Some(score / self.painted as f32)
    }

    /// One finished model call, as the provider reported it — the strip's input.
    ///
    /// One call is not one tile any more: it becomes the reading the front writes, and how many tiles
    /// it ends up covering is how long it stayed the newest thing known. The context figures follow
    /// the **last** call, because they answer "how does the context look right now", and an older
    /// call's numbers describe a shorter context that no longer exists. Unlike [`Self::observe_turn`]
    /// this is not a sum, so `cached` really is a share of `prompt` and may safely be compared
    /// against it.
    pub fn observe_call(&mut self, u: &Usage, last_prompt: Option<u64>) {
        let Some(prompt) = u.prompt.or(last_prompt) else {
            return; // the endpoint reported nothing at all for this call
        };
        if prompt == 0 {
            return;
        }
        // Drop the estimate *before* applying: the measurement describes the same request, so the
        // reading must be computed from it alone — otherwise the window would keep the estimate's
        // extra length for the rest of the conversation, since nothing else ever clears it.
        self.est = 0;
        self.apply(prompt, u.cached, u.cache_miss);
        self.call_seen = true;
        if let Some(cached) = u.cached {
            // No cache field, no reading: a blank strip is a truthful "nothing measured", whereas a
            // field of grey misses would be an invention. `has_cache` carries that state to the
            // header. The calls counter still ticks, because "Call #n" is about the conversation.
            self.calls = self.calls.saturating_add(1);
            self.latest = Some(Rec::call(self.calls, prompt, cached));
        }
    }

    /// The turn's aggregate ledger — the line that lands in the conversation file.
    ///
    /// It is the only thing that can report the lifetime figures (a per-call event cannot know that
    /// it was the last of its turn), so those are always taken from it. The reading is another
    /// matter — see [`Self::call_seen`]: a turn that already reported per-call figures must not
    /// replace them with its own sum, because that sum counts tokens from calls that are over and
    /// would make the picture disagree with the request it claims to describe.
    pub fn observe_turn(&mut self, rec: &UsageRecord) {
        let u = &rec.usage;
        let Some(prompt) = u.prompt.or(rec.last_prompt) else {
            return;
        };
        if prompt == 0 {
            return;
        }
        self.saved += u.cached.unwrap_or(0);
        self.prompt_total += prompt;
        self.rounds = self.rounds.max(rec.round);
        if u.cached.is_some() || u.cache_miss.is_some() {
            self.has_cache = true;
        }
        if !self.call_seen {
            // No per-call figures this turn: this aggregate is the only reading there is. The hit is
            // clamped inside `apply`, because a multi-call turn's sum really can overshoot `prompt`.
            self.apply(prompt, u.cached, u.cache_miss);
            if let Some(cached) = u.cached {
                self.latest = Some(Rec::round(rec.round, rec.calls, prompt, cached));
            }
        }
        // The turn is over, so nothing is in flight: the provisional frontier has served its purpose
        // whichever branch ran above, and leaving it up would make an idle window disagree with the
        // figures printed beside it.
        self.est = 0;
        self.call_seen = false;
    }

    /// Pick up a conversation's ledger, so the strip starts from the last thing that was measured.
    ///
    /// Opening an old conversation cannot rebuild the picture: the file records one aggregate per
    /// turn and no timestamps, so 20 rounds are 20 readings with no idea how long each one lasted —
    /// and the strip is 25 seconds wide, not 20 rounds wide. Inventing a shape for them would be
    /// exactly the kind of decoration this window is supposed to avoid. So the newest aggregate
    /// becomes the reading the front writes, the strip starts blank, and the picture a reopened
    /// conversation shows is the truth of what happened next rather than a guess about what came
    /// before.
    ///
    /// `session_calls` is still picked up, so the next live call keeps counting from where the
    /// conversation really is.
    pub fn replay(&mut self, log: &[UsageRecord]) {
        for rec in log {
            self.observe_turn(rec);
        }
        if let Some(last) = log.last() {
            self.calls = self.calls.max(last.session_calls);
        }
    }

    /// Shared body of the two observers: everything except the lifetime totals and the reading.
    fn apply(&mut self, prompt: u64, cached: Option<u64>, cache_miss: Option<u64>) {
        self.seen = true;
        self.used = prompt;

        match cached {
            Some(c) => {
                self.has_cache = true;
                self.hit = c.min(prompt);
            }
            None => {
                // No cache field: the whole prompt was computed, but do not claim "0% hit" for an
                // endpoint that simply does not report caching — `has_cache` keeps the guess out.
                self.hit = 0;
            }
        }
        // DeepSeek reports the miss explicitly; elsewhere it is whatever the hit did not cover.
        self.miss = cache_miss.unwrap_or(prompt - self.hit).min(prompt - self.hit);
    }

    /// The breakdown of the request into sections, reported by the agent thread **before each
    /// call**. Also the only signal that exists while that call is in flight, so its total doubles
    /// as the provisional frontier (see [`Self::est`]).
    pub fn observe_context(&mut self, segs: Vec<Seg>) {
        self.est = segs.iter().map(|s| s.tokens).sum();
        self.segs = segs;
        // A new request is going out, so last round's per-call figure no longer describes it.
        self.call_seen = false;
    }

    /// Clear everything (switching or starting a conversation).
    ///
    /// The window itself **stays open**: it is a lens on "the conversation I am in", not part of any
    /// one conversation, and having it disappear when you switch conversations would be surprising.
    /// `gw.forget_placement` is deliberately not called either — the window must not jump back to
    /// the centre of the screen on every conversation switch.
    ///
    /// The strip goes with it. Leaving the previous conversation's tiles on screen would be the worst
    /// kind of wrong: a picture that looks like a reading of the conversation you are in.
    pub fn reset(&mut self) {
        self.used = 0;
        self.hit = 0;
        self.miss = 0;
        self.est = 0;
        self.cells = [Rec::EMPTY; BLOCKS as usize];
        self.front = 0;
        self.painted = 0;
        self.sweep = 0.0;
        self.latest = None;
        self.calls = 0;
        self.segs.clear();
        self.saved = 0;
        self.prompt_total = 0;
        self.rounds = 0;
        self.has_cache = false;
        self.seen = false;
        self.call_seen = false;
    }

    // ── Derived figures ──

    /// The context size the window reads against: the last **measured** occupancy, or the estimate
    /// for the request in flight when that is larger.
    fn width(&self) -> u64 {
        self.used.max(self.est)
    }

    /// Hit ratio of the **current context**, 0.0..=1.0. `None` when nothing has been measured.
    ///
    /// The denominator is [`Self::width`], so while a call is in flight the figure temporarily holds
    /// the newly added context against the previous call's hits — which is precisely the question
    /// that call is about to answer, and reads as a dip that the reply then resolves upward.
    ///
    /// `seen` is the gate rather than `width`, and the difference matters on the first call of a
    /// conversation: there is an estimate of the request size but no answer yet about any of it, and
    /// printing `0.0%` there would be a confident reply to a question nobody has been asked.
    fn ratio(&self) -> Option<f32> {
        if !self.seen {
            return None;
        }
        let w = self.width();
        if w == 0 {
            None
        } else {
            Some(self.hit as f32 / w as f32)
        }
    }

    /// Lifetime hit ratio over the whole conversation.
    fn lifetime_ratio(&self) -> Option<f32> {
        if self.prompt_total == 0 {
            None
        } else {
            Some(self.saved as f32 / self.prompt_total as f32)
        }
    }
}

pub type Shared = Arc<Mutex<CacheView>>;

pub fn new_shared() -> Shared {
    Arc::new(Mutex::new(CacheView::default()))
}

/// Take the lock. A previous panic poisons it; recover directly here — one window crashing should
/// not take the other windows down with it.
pub fn lock(a: &Shared) -> MutexGuard<'_, CacheView> {
    a.lock().unwrap_or_else(|e| e.into_inner())
}

fn cache_id() -> ViewportId {
    ViewportId::from_hash_of("aox-cache")
}

/// The window title. Used in two places: reported when building the viewport, and later used to
/// find the HWND back by title to set rounded corners, so there must be exactly one copy.
const WINDOW_TITLE: &str = "AOX Cache Hit Map";

/// Open the window (idempotent; the settings page's button is not a toggle — see [`Gwin::open`]).
pub fn open(ctx: &egui::Context, a: &Shared) {
    lock(a).gw.open(ctx);
}

/// Called once per frame by the main window.
pub fn sync(ctx: &egui::Context, a: &Shared) {
    let mut st = lock(a);
    if !st.gw.open {
        drop(st);
        gwin::want(gwin::WIN_CACHE, false);
        return;
    }
    // A touch smaller than the activity stream: the grid is square and the statistics column is
    // fixed-width, so a full-size window would only add empty margin around the grid.
    let size = gwin::initial_size(ctx) * 0.9;
    let size = Vec2::new(size.x.max(MIN_W), size.y.max(MIN_H));
    // Offset from dead centre, so that having both windows open is legible: they would otherwise
    // be declared with the same position and the second would look like the first unchanged.
    let b = st.gw.builder(ctx, WINDOW_TITLE, size, Vec2::new(MIN_W, MIN_H), Vec2::new(46.0, 34.0));

    let a2 = a.clone();
    drop(st);
    ctx.show_viewport_deferred(cache_id(), b, move |ctx, _| window(ctx, &a2));
}

fn window(ctx: &egui::Context, shared: &Shared) {
    let mut st = lock(shared);

    st.gw.prelude(ctx, WINDOW_TITLE);

    // The front runs on the frame clock of **this** window, never on the main window's job clock.
    //
    // `tick` deliberately drives nothing that moves here. `now` only advances as often as the main
    // window repaints, which for an idle program is rarely, so anything timed off it arrives in
    // whatever rhythm the other window happened to have — which is precisely what made an older
    // version read as stutter. Here it is the delta between the last two frames of the window that
    // is actually drawing, so the front is as smooth as that window renders and independent of the
    // rest of the program.
    st.advance(ctx.input(|i| i.stable_dt));

    let frame = Frame::NONE.fill(P::panel());
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        // Child and main viewports share one `egui::Context`, so widget Ids are **not**
        // automatically prefixed with viewport info.
        ui.push_id("aox-cache-window", |ui| {
            body(ui, ctx, &mut st);
        });
    });

    // See the matching note in `activity::window`: the lock must be gone before the poke.
    let d = st.gw.step();
    let open = st.gw.open;
    // Frames are asked for only while the front is walking, because nothing else in this window
    // moves. A turn in flight needs a real 60 fps (the frame pulses, and the strip is 98 ms a tile);
    // a stopped window needs none at all — it is a still picture, and asking anyway would wake the
    // main window on every tick to re-lay-out its transcript for a picture that did not change.
    //
    // Idle, waiting on a choice, waiting on a permission: all three are "the next reading is not
    // coming until the user does something", and all three freeze here (`advance` checks the same
    // flag).
    let busy = st.gw.busy;
    drop(st);
    gwin::drive(ctx, gwin::WIN_CACHE, d);

    // A child's own repaint request: it schedules a pass for this viewport alone and deliberately
    // does not poke the main window, whose pass is expensive (it re-lays-out the transcript) and is
    // not what draws the tile. Gated on `open` as well: once the switch is flipped this window is on
    // its way out, and a self-repainting child is exactly what would keep it on screen (see
    // `gwin::step`).
    if open && busy {
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

/// Height reserved for the "context sections" strip along the bottom.
const FOOT_H: f32 = 112.0;
/// Radius of a grid cell. Small on purpose: at 16×16 the cells are ~20 points across, and a
/// generous radius turns the grid into a field of pills.
const CELL_R: u8 = 3;
/// Height of the column-number band under the grid.
const AXIS_H: f32 = 18.0;

/// The colours the grid is painted with, resolved **per frame** from the active theme.
///
/// Taken from the sheet this window was modelled on rather than from the program's own palette, and
/// deliberately so. Three states have to be readable *against each other* on 20-point tiles: near
/// black, a muted gold, a soft brick. The first two schemes both failed on that — a cache-green hit
/// against a tan miss is one step apart in lightness, and three greys are one hue apart at all — so
/// the ladder now runs dark → gold → brick, which separates all three by **hue** and by **level** at
/// once. The gold is deliberately low in saturation: a clean yellow at this size reads as a warning
/// sticker, and the state it stands for is "the prefix came back, the tail did not", not "error".
#[derive(Clone, Copy)]
struct Palette {
    hit: Color32,
    partial: Color32,
    miss: Color32,
    /// An unlit step of the context's 16-step bar under the headline. Not used by the grid: a
    /// position the front has not reached is left as bare panel (see [`CacheView::has`]), because a
    /// faint square and a "this is where the strip stops" edge are two different statements and the
    /// grid only ever makes the second one.
    faint: Color32,
    /// Stroke of the newest tile and of the hovered cell.
    ring: Color32,
}

impl Palette {
    fn of(self, v: Verdict) -> Color32 {
        match v {
            Verdict::Hit => self.hit,
            Verdict::Partial => self.partial,
            Verdict::Miss => self.miss,
        }
    }
}

fn palette() -> Palette {
    match theme::mode() {
        theme::Mode::Light => Palette {
            hit: Color32::from_rgb(0x14, 0x14, 0x14),
            // A muted gold, not a mid grey: grey said "somewhere in between" and could be mistaken
            // for a very old hit (the fade ends at 30%, which is a light grey on this panel).
            partial: Color32::from_rgb(0xC4, 0xB0, 0x77),
            miss: Color32::from_rgb(0xB2, 0x7B, 0x73),
            // One full step below `P::panel`, which is what the window is painted on: an unlit step of
            // the context bar has to read as *a step*, not as a hole in the bar.
            faint: Color32::from_rgb(0xE0, 0xDC, 0xD4),
            ring: P::text(),
        },
        theme::Mode::Dark => Palette {
            hit: Color32::from_rgb(0xED, 0xED, 0xEB),
            // Same idea, one step down: on this panel a light gold would out-shine the hit, which is
            // the state that should dominate — but not so far down that it reads as olive. Low
            // saturation keeps it a state, not a warning.
            partial: Color32::from_rgb(0xB4, 0xA3, 0x5F),
            miss: Color32::from_rgb(0xC0, 0x8F, 0x87),
            // The dark panel sits at #191917 rather than the reference's near-black, so the unlit step
            // is lifted with it: the same *step* above the panel is what keeps the bar legible.
            faint: Color32::from_rgb(0x2A, 0x2A, 0x28),
            ring: P::text(),
        },
    }
}

fn body(ui: &mut Ui, ctx: &egui::Context, st: &mut CacheView) {
    // The whole window's rect. **Must be taken before putting anything into the ui**: `max_rect()`
    // shrinks with each allocate, and by the end only a sliver at the bottom would be left.
    let full = ui.max_rect();

    let info = header_info(st);
    gwin::header(ui, ctx, &mut st.gw, aox::i18n::t("Cache Hit Map"), &info);

    // Which tile the cursor is over, and where it is. Filled in while the grid draws and consumed
    // after the layout has finished: a card floating over the grid must not take part in laying it
    // out, or the grid would jump the moment the cursor entered it.
    let mut hover: Option<(u32, Pos2)> = None;

    Frame::NONE.inner_margin(Margin { left: 18, right: 18, top: 12, bottom: 14 }).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;

        let main_h = (ui.available_height() - FOOT_H - 14.0).max(220.0);
        let left_w = (ui.available_width() - SIDE_W - SIDE_GAP).max(260.0);

        ui.horizontal_top(|ui| {
            // Hand-written spacing: `item_spacing` would otherwise stack between every allocate and
            // quietly make the row wider than the window.
            ui.spacing_mut().item_spacing.x = 0.0;

            ui.vertical(|ui| {
                ui.set_width(left_w);
                ui.set_height(main_h);
                title_row(ui, st, left_w);
                ui.add_space(12.0);
                let size = Vec2::new(left_w, ui.available_height().max(140.0));
                hover = grid(ui, st, size);
            });

            ui.add_space(SIDE_GAP);

            ui.vertical(|ui| {
                ui.set_width(SIDE_W);
                stats_card(ui, st);
                ui.add_space(10.0);
                legend_card(ui);
            });
        });

        ui.add_space(14.0);
        sections(ui, st);
    });

    if let Some((tile, at)) = hover {
        tooltip(ctx, st, tile, at, full);
    }

    // Overlaid last, so the border hot-zones beat the top bar's drag zone (see `gwin`).
    gwin::resize_handles(ui, ctx, full);
    gwin::outline(ui, full);
}

/// The header's one-line state readout. Built here rather than inside `gwin::header`: the shell has
/// no idea what a given window is measuring.
///
/// Every float is formatted **before** it reaches `aox::tf!`, which fills bare `{}` only — a
/// `{:.1}` handed to it would be printed literally.
fn header_info(st: &CacheView) -> String {
    let Some(r) = st.ratio() else {
        return aox::i18n::t("Idle · no call observed").to_string();
    };
    let pct = format!("{:.1}", r * 100.0);
    if st.gw.busy {
        aox::tf!("Busy · {}% cached", pct)
    } else {
        aox::tf!("Idle · {} rounds · {}% cached", st.rounds, pct)
    }
}

// ───────────────────────────── The grid ─────────────────────────────

/// Draw the 16×16 grid inside an area of `size`, and report which tile the cursor is over.
///
/// Everything here is hand-laid-out rather than composed from widgets: the row numbers, the column
/// numbers and the 256 tiles all have to line up to the pixel, and a layout that recomputes cell
/// size from whatever the widgets ended up wanting is exactly how a chart ends up one pixel off on
/// the fifth column only.
fn grid(ui: &mut Ui, st: &CacheView, size: Vec2) -> Option<(u32, Pos2)> {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if resp.hovered() {
        // A pointer over the grid asks a question; a text cursor is the wrong answer.
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }
    let p = ui.painter();
    let pal = palette();
    let n = GRID as f32;

    // Cell size: the largest square that fits, in both directions. Clamped at the top so a huge
    // window does not turn 256 tiles into 256 postage stamps each.
    let cell = ((size.x - GUTTER - (n - 1.0) * CELL_GAP) / n)
        .min((size.y - AXIS_H - (n - 1.0) * CELL_GAP) / n)
        .clamp(6.0, 34.0);
    let span = n * cell + (n - 1.0) * CELL_GAP;
    // Centred in whatever room `size` gives: a square grid pinned to the top-left corner of a
    // generous window reads as a layout mistake, and centring costs nothing because the cell size
    // is already decided.
    let x0 = rect.left() + GUTTER + ((size.x - GUTTER - span) * 0.5).max(0.0);
    let y0 = rect.top() + ((size.y - AXIS_H - span) * 0.5).max(0.0);

    let mouse = ui.ctx().input(|i| i.pointer.hover_pos());
    let mut hover = None;

    // Fill pass. The cells are laid out row by row — that is just geometry — but each one holds the
    // reading the front painted there (see `flow_of`), which is what makes the strip read as one
    // thread of time instead of as a scatter of state.
    for r in 0..GRID {
        for c in 0..GRID {
            let i = flow_of(r, c);
            let at = Pos2::new(x0 + c as f32 * (cell + CELL_GAP), y0 + r as f32 * (cell + CELL_GAP));
            let cr = Rect::from_min_size(at, Vec2::splat(cell));
            // A position the front has never reached has no tile at all — the panel shows through —
            // so the first lap is the square being drawn rather than a blank grid filling in. After
            // that every cell has content for ever, and the picture rolls instead of resetting.
            if let Some(rec) = st.at(i) {
                // Strength *is* the age: full on the tile just written, fading linearly to 30% at
                // the far end of the walk. Nothing else encodes time, so this one number has to be
                // right — a band's edge is where a reading gave way to the next.
                p.rect_filled(cr, CornerRadius::same(CELL_R), pal.of(rec.verdict).gamma_multiply(st.fade(i)));
            }

            // Hover is gated with the fill, in the same test: a cell the front has not reached is not
            // a cell you can ask about, and a tooltip over a tile that is not drawn would be a
            // reading out of nothing.
            if st.has(i) && cr.contains(mouse.unwrap_or(Pos2::new(f32::MIN, f32::MIN))) {
                hover = Some(i);
            }
        }
    }

    // The frame, drawn after the fills so it stands on top of them and before the hover ring so
    // hovering a tile is never hidden by it. It sits on the tile the front has **just written** —
    // the brightest one — which is the only position on the strip that means "now".
    if let Some(now) = st.now_cell() {
        let (hr, hc) = cell_of(now);
        let cr = Rect::from_min_size(
            Pos2::new(x0 + hc as f32 * (cell + CELL_GAP), y0 + hr as f32 * (cell + CELL_GAP)),
            Vec2::splat(cell),
        );
        newest_frame(p, cr, st.gw.busy, st.pulse);
    }

    if let Some(i) = hover {
        let (hr, hc) = cell_of(i);
        let cr = Rect::from_min_size(
            Pos2::new(x0 + hc as f32 * (cell + CELL_GAP), y0 + hr as f32 * (cell + CELL_GAP)),
            Vec2::splat(cell),
        );
        p.rect_stroke(
            cr,
            CornerRadius::same(CELL_R),
            Stroke::new(1.4, pal.ring),
            egui::StrokeKind::Outside,
        );
    }

    // Row and column numbers, 1-based. Plain coordinates now: on a row-major grid they would double
    // as a reading order, but the walk turns at every scale, so the only order is the curve itself
    // and the numbers are here just to locate a tile when one is being pointed at.
    let f = FontId::proportional(10.5);
    for r in 0..GRID {
        let y = y0 + r as f32 * (cell + CELL_GAP) + cell * 0.5;
        p.text(
            Pos2::new(x0 - 8.0, y),
            Align2::RIGHT_CENTER,
            (r + 1).to_string(),
            f.clone(),
            P::text_faint(),
        );
    }
    for c in 0..GRID {
        let x = x0 + c as f32 * (cell + CELL_GAP) + cell * 0.5;
        p.text(
            Pos2::new(x, y0 + span + 6.0),
            Align2::CENTER_TOP,
            (c + 1).to_string(),
            f.clone(),
            P::text_faint(),
        );
    }

    hover.map(|i| (i, mouse.unwrap_or(rect.center())))
}

/// The frame on the newest tile: a box that sits **around** a cell rather than pointing at it.
///
/// One plain frame, and nothing else: no arrow, no glyph. A frame can be read straight off the grid
/// — it is *at* the position — whereas a pointer has to be aimed, and aiming one correctly depends
/// on knowing which way the walk runs, which is not something a reader of the picture should have to
/// work out first.
///
/// It is outset into the cell **gap** rather than inset into the tile. An inset ring darkens the one
/// tile whose colour you most want to read — and the newest tile is the one you are looking at.
///
/// Painted in the theme's foreground rather than in any of the three state colours: the box stands
/// on all of them, and a ring in the hit colour sitting on a hit tile is invisible exactly where it
/// matters most. The pulse is reserved for "a turn is running", the same rule the rest of the window
/// follows — a mark that blinks in an idle window claims something is happening.
fn newest_frame(p: &egui::Painter, cr: Rect, busy: bool, pulse: f32) {
    let a = if busy {
        if (pulse * 1.6) as i32 % 2 == 0 {
            1.0
        } else {
            0.55
        }
    } else {
        0.9
    };
    // Sits in the cell gap, with a sliver of panel colour on **both** sides of it. That sliver is
    // the whole trick: a frame drawn flush against the tile merges with it whenever the two are
    // close in value — a light tile and a light frame in the dark theme — and a frame that never
    // touches the tile reads as a frame in either theme.
    let grow = CELL_GAP * 0.5 + 0.25;
    p.rect_stroke(
        cr.expand(grow),
        CornerRadius::same(CELL_R + grow.ceil() as u8),
        Stroke::new(1.4, P::text().gamma_multiply(a)),
        egui::StrokeKind::Middle,
    );
}

/// The line above the grid: what the grid is, on the left; the headline number on the right.
fn title_row(ui: &mut Ui, st: &CacheView, w: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 46.0), Sense::hover());
    let p = ui.painter();
    p.text(
        rect.left_top(),
        Align2::LEFT_TOP,
        aox::i18n::t("Cache hit status"),
        FontId::proportional(14.0),
        P::text(),
    );
    let sub = aox::tf!("{} × {} tiles · one per 98 ms", GRID, GRID);
    p.text(
        rect.left_top() + Vec2::new(0.0, 21.0),
        Align2::LEFT_TOP,
        sub,
        FontId::proportional(12.0),
        P::text_faint(),
    );

    // `—` rather than `0.0%` before the first tile: those are different statements, and only one of
    // them is true when nothing has been measured. The picture's own weighted rate, so the number
    // and the picture can never disagree — see [`CacheView::map_ratio`].
    let big = match st.map_ratio() {
        Some(r) => format!("{:.1}%", r * 100.0),
        None => "—".to_string(),
    };
    p.text(rect.right_top(), Align2::RIGHT_TOP, big, theme::bold_font(26.0), P::text());
    p.text(
        rect.right_top() + Vec2::new(0.0, 31.0),
        Align2::RIGHT_TOP,
        aox::i18n::t("Hit rate"),
        FontId::proportional(12.0),
        P::text_faint(),
    );
}

// ───────────────────────────── The right column ─────────────────────────────

fn stats_card(ui: &mut Ui, st: &CacheView) {
    let pal = palette();
    // Counted off the strip, not off the context: these three rows are a caption for the picture, so
    // they are the picture. A tile is 98 ms here, which is why they add up to the number of painted
    // tiles and can be checked against the grid by eye — and why a percentage is a share of *time*
    // rather than a share of calls.
    let (hits, partials, misses) = st.tally();
    let n = hits + partials + misses;
    let pct = |k: u32| {
        if n == 0 {
            None
        } else {
            Some(format!("{:.1}%", f64::from(k) * 100.0 / f64::from(n)))
        }
    };

    theme::card(10)
        .inner_margin(Margin { left: 12, right: 12, top: 11, bottom: 10 })
        .show(ui, |ui| {
            ui.set_width(SIDE_W - 24.0);
            ui.spacing_mut().item_spacing.y = 0.0;

            let (head, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
            ui.painter().text(
                head.left_center(),
                Align2::LEFT_CENTER,
                aox::i18n::t("Cache stats"),
                FontId::proportional(13.5),
                P::text(),
            );

            let w = ui.available_width();
            stat_row(ui, pal.hit, aox::i18n::t("Hit"), hits, pct(hits), w);
            stat_row(ui, pal.partial, aox::i18n::t("Partial"), partials, pct(partials), w);
            stat_row(ui, pal.miss, aox::i18n::t("Miss"), misses, pct(misses), w);

            ui.add_space(8.0);
            rule(ui, w);
            ui.add_space(8.0);

            // These three are about the **context**, not the strip: how much of the request that just
            // went out came back from the cache, and what the whole conversation has saved.
            kv_line(
                ui,
                aox::i18n::t("This call"),
                st.ratio().map_or("—".into(), |r| format!("{:.1}%", r * 100.0)),
                w,
            );
            kv_line(ui, aox::i18n::t("Tokens saved"), thousands(st.saved), w);
            kv_line(
                ui,
                aox::i18n::t("Lifetime hit rate"),
                st.lifetime_ratio().map_or("—".into(), |r| format!("{:.1}%", r * 100.0)),
                w,
            );
        });
}

fn stat_row(ui: &mut Ui, dot: Color32, name: &str, count: u32, pct: Option<String>, w: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 21.0), Sense::hover());
    let p = ui.painter();
    p.circle_filled(Pos2::new(r.left() + 4.0, r.center().y), 4.0, dot);
    p.text(
        Pos2::new(r.left() + 15.0, r.center().y),
        Align2::LEFT_CENTER,
        name,
        FontId::proportional(12.5),
        P::text_dim(),
    );
    p.text(
        Pos2::new(r.right() - 46.0, r.center().y),
        Align2::RIGHT_CENTER,
        count.to_string(),
        FontId::proportional(12.5),
        P::text(),
    );
    p.text(
        Pos2::new(r.right(), r.center().y),
        Align2::RIGHT_CENTER,
        pct.unwrap_or_else(|| "—".to_string()),
        theme::mono_font(11.5),
        P::text_faint(),
    );
}

fn kv_line(ui: &mut Ui, key: &str, val: String, w: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 19.0), Sense::hover());
    let p = ui.painter();
    p.text(
        Pos2::new(r.left(), r.center().y),
        Align2::LEFT_CENTER,
        key,
        FontId::proportional(12.0),
        P::text_faint(),
    );
    p.text(
        Pos2::new(r.right(), r.center().y),
        Align2::RIGHT_CENTER,
        val,
        FontId::proportional(12.5),
        P::text(),
    );
}

fn rule(ui: &mut Ui, w: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter().hline(r.x_range(), r.center().y, Stroke::new(1.0, P::border()));
}

fn legend_card(ui: &mut Ui) {
    let pal = palette();
    theme::card(10)
        .inner_margin(Margin { left: 12, right: 12, top: 11, bottom: 10 })
        .show(ui, |ui| {
            ui.set_width(SIDE_W - 24.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            let (head, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
            ui.painter().text(
                head.left_center(),
                Align2::LEFT_CENTER,
                aox::i18n::t("Legend"),
                FontId::proportional(13.5),
                P::text(),
            );
            let w = ui.available_width();
            legend_row(ui, pal.hit, aox::i18n::t("Hit"), aox::i18n::t("Served from the prompt cache"), false, w);
            legend_row(
                ui,
                pal.partial,
                aox::i18n::t("Partial"),
                aox::i18n::t("Prefix reused, the new tail not"),
                false,
                w,
            );
            legend_row(
                ui,
                pal.miss,
                aox::i18n::t("Miss"),
                aox::i18n::t("Cold start, or an expired cache"),
                false,
                w,
            );
            // Fade is not a fourth state and gets no swatch of its own — it is what *age* looks
            // like, and it is explained by the row that introduces the frame. The frame sits on the
            // tile the front has just written, which holds whatever was measured most recently; the
            // tiles behind it were painted earlier and are dimmer for it.
            legend_row(
                ui,
                Color32::TRANSPARENT,
                aox::i18n::t("Newest"),
                aox::i18n::t("Newest reading · older fade"),
                true,
                w,
            );
        });
}

fn legend_row(ui: &mut Ui, dot: Color32, name: &str, desc: &str, head: bool, w: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 31.0), Sense::hover());
    let p = ui.painter();
    let sq = Rect::from_min_size(Pos2::new(r.left() + 1.0, r.top() + 4.0), Vec2::splat(11.0));
    if head {
        // The very mark the grid draws, so the legend is a key to the picture rather than a
        // description of it — anything else and a reader has to map one shape onto another.
        //
        // The tile under the frame is a neutral one rather than any of the three states: the frame
        // is not a fourth state, it can stand on all of them, and showing it over one of them here
        // would read as a claim about which.
        p.rect_filled(sq, CornerRadius::same(CELL_R), P::surface_hi());
        newest_frame(p, sq, false, 0.0);
    } else {
        p.rect_filled(sq, CornerRadius::same(CELL_R), dot);
    }
    p.text(
        Pos2::new(r.left() + 20.0, r.top()),
        Align2::LEFT_TOP,
        name,
        FontId::proportional(12.0),
        P::text_dim(),
    );
    // Elided rather than wrapped: a legend line that folds onto a second row pushes the row below
    // it down, and four legend rows of varying height read as four unrelated things. One line each,
    // truncated with an ellipsis if the column is narrow.
    let desc_font = FontId::proportional(11.5);
    let g = gwin::elide(ui, desc, desc_font, P::text_faint(), w - 20.0);
    p.galley(Pos2::new(r.left() + 20.0, r.top() + 15.0), g, P::text_faint());
}

// ───────────────────────── The context breakdown ─────────────────────────

/// The strip along the bottom: the request split into the sections the host assembled it from,
/// each with its own hit ratio.
///
/// This is the one part of the window whose numbers come from the **agent thread** rather than from
/// the usage record — only that side knows how the request was built. Until it reports (the first
/// frame after launch, or an endpoint with no usage at all) the strip says so instead of drawing
/// four empty bars.
fn sections(ui: &mut Ui, st: &CacheView) {
    let w = ui.available_width();
    let (head, _) = ui.allocate_exact_size(Vec2::new(w, 20.0), Sense::hover());
    ui.painter().text(
        head.left_top(),
        Align2::LEFT_TOP,
        aox::i18n::t("Context sections"),
        FontId::proportional(13.5),
        P::text_dim(),
    );
    ui.add_space(8.0);

    if st.segs.is_empty() {
        ui.label(
            RichText::new(aox::i18n::t("Waiting for the agent to report this request"))
                .size(12.0)
                .color(P::text_faint()),
        );
        return;
    }

    // Walk the sections in assembly order, accumulating their token ranges: section k starts where
    // k-1 ended, which is what makes "how much of *this* section was cached" answerable at all.
    let mut cursor = 0u64;
    let marks: Vec<(&'static str, u64, u64)> = st
        .segs
        .iter()
        .map(|s| {
            let start = cursor;
            cursor += s.tokens;
            let hit = st.hit.saturating_sub(start).min(s.tokens);
            (s.label, s.tokens, hit)
        })
        .collect();

    let n = marks.len() as f32;
    let col_w = ((w - (n - 1.0) * 14.0) / n).max(80.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        for (label, tokens, hit) in marks {
            ui.vertical(|ui| {
                ui.set_width(col_w);
                section_col(ui, label, tokens, hit, col_w);
            });
        }
    });
}

fn section_col(ui: &mut Ui, label: &'static str, tokens: u64, hit: u64, w: f32) {
    let pal = palette();
    let ratio = if tokens == 0 { 0.0 } else { hit as f32 / tokens as f32 };
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 72.0), Sense::hover());
    let p = ui.painter();

    p.text(
        rect.left_top(),
        Align2::LEFT_TOP,
        aox::i18n::t(label),
        FontId::proportional(12.5),
        P::text(),
    );
    let pct = format!("{:.1}", ratio * 100.0);
    p.text(
        rect.left_top() + Vec2::new(0.0, 19.0),
        Align2::LEFT_TOP,
        aox::tf!("{}% cached", pct),
        FontId::proportional(11.5),
        P::text_faint(),
    );

    // A 16-step bar rather than a continuous one: the grid above is discrete blocks, and a smooth
    // progress bar next to it would imply a precision the numbers do not have.
    const N: usize = 16;
    let gap = 2.0;
    let cw = ((w - (N as f32 - 1.0) * gap) / N as f32).max(2.0);
    let y = rect.top() + 38.0;
    for i in 0..N {
        let on = (i as f32 + 0.5) / N as f32 <= ratio;
        let cr = Rect::from_min_size(Pos2::new(rect.left() + i as f32 * (cw + gap), y), Vec2::new(cw, 8.0));
        p.rect_filled(cr, CornerRadius::same(2), if on { pal.hit } else { pal.faint });
    }

    p.text(
        Pos2::new(rect.left(), y + 15.0),
        Align2::LEFT_TOP,
        aox::tf!("{} / {} tokens", thousands(hit.min(tokens)), thousands(tokens)),
        FontId::proportional(11.5),
        P::text_faint(),
    );
}

// ───────────────────────────── The tooltip ─────────────────────────────

/// The card that follows the cursor over the grid: one call, its own figures, and the band those
/// figures fell into.
///
/// Everything on it is a measurement except the badge — and the badge is immediately checkable,
/// because the share it was derived from is printed two rows below it. That is the rule this window
/// is built on: a label may summarise a number, it may never be the only copy of one.
fn tooltip(ctx: &egui::Context, st: &CacheView, tile: u32, at: Pos2, full: Rect) {
    // An unwritten tile has nothing to say, and a "no data" card would put a floating panel under
    // the cursor for most of an empty grid — which reads as a bug, not as an answer.
    let Some(rec) = st.at(tile) else {
        return;
    };
    let pal = palette();
    const W: f32 = 248.0;
    const H: f32 = 138.0;

    // Prefer the lower-right of the cursor, flip when there is no room, and clamp into the window
    // so the card can never be pushed off-screen by a tile in the corner.
    let mut x = at.x + 18.0;
    if x + W > full.right() - 6.0 {
        x = at.x - W - 18.0;
    }
    let x = x.clamp(full.left() + 6.0, (full.right() - W - 6.0).max(full.left() + 6.0));
    let y = (at.y + 14.0).clamp(full.top() + 6.0, (full.bottom() - H - 6.0).max(full.top() + 6.0));

    let badge = match rec.verdict {
        Verdict::Hit => aox::i18n::t("Hit"),
        Verdict::Partial => aox::i18n::t("Partial"),
        Verdict::Miss => aox::i18n::t("Miss"),
    };
    let badge_bg = pal.of(rec.verdict);
    // The chip has to stay legible on all three verdict colours, so its label is picked by the
    // chip's own lightness rather than by the theme: `Hit` is near-black in one theme and near-white
    // in the other, and a fixed foreground would vanish on one of them.
    let c = badge_bg.to_array();
    let luma = (u32::from(c[0]) * 299 + u32::from(c[1]) * 587 + u32::from(c[2]) * 114) / 1000;
    let badge_fg = if luma > 140 {
        Color32::from_rgb(0x14, 0x14, 0x14)
    } else {
        Color32::from_rgb(0xF5, 0xF3, 0xEE)
    };

    let title = match rec.unit {
        Unit::Call => aox::tf!("Call #{}", rec.seq),
        Unit::Round => aox::tf!("Round #{} · {} calls", rec.seq, rec.calls),
    };
    // Age in seconds, converted from the distance behind the front: the strip's own clock *is* the
    // distance, so this is a conversion rather than a second timestamp to keep in sync. It reads as
    // "how long ago this tile was written", which is the same thing as "how long this reading has
    // been on the picture".
    let behind = st.behind(tile);
    let back = if behind == 0 {
        aox::i18n::t("the newest tile").to_string()
    } else {
        aox::tf!("{}s ago", format!("{:.1}", behind as f32 * TILE_SECS))
    };

    egui::Area::new(egui::Id::new("aox-cache-pop"))
        .order(Order::Foreground)
        .fixed_pos(Pos2::new(x, y))
        .show(ctx, |ui| {
            Frame::NONE
                .fill(P::surface())
                .corner_radius(CornerRadius::same(theme::card_radius()))
                .stroke(Stroke::new(1.0, P::edge_raised()))
                .shadow(theme::card_shadow())
                .inner_margin(Margin::symmetric(14, 11))
                .show(ui, |ui| {
                    ui.set_width(W - 28.0);
                    ui.spacing_mut().item_spacing.y = 0.0;

                    let (head, _) =
                        ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
                    let p = ui.painter();
                    let f = FontId::proportional(13.0);
                    p.text(
                        Pos2::new(head.left(), head.center().y),
                        Align2::LEFT_CENTER,
                        title,
                        f,
                        P::text(),
                    );
                    let bw = gwin::text_w(ui, &FontId::proportional(11.0), badge) + 14.0;
                    let br = Rect::from_min_size(
                        Pos2::new(head.right() - bw, head.center().y - 8.0),
                        Vec2::new(bw, 16.0),
                    );
                    p.rect_filled(br, CornerRadius::same(4), badge_bg);
                    p.text(br.center(), Align2::CENTER_CENTER, badge, FontId::proportional(11.0), badge_fg);

                    ui.add_space(8.0);
                    tip_row(ui, aox::i18n::t("Prompt tokens"), thousands(rec.prompt));
                    tip_row(ui, aox::i18n::t("Cached tokens"), thousands(rec.cached));
                    // The number the badge was banded from, printed so the band can be argued with.
                    tip_row(ui, aox::i18n::t("Cached share"), format!("{:.1}%", rec.share() * 100.0));
                    // Age, because the fade already encodes it: a faint tile is an old one, and the
                    // card is where that turns into a number.
                    tip_row(ui, aox::i18n::t("Age"), back);
                });
        });
}

fn tip_row(ui: &mut Ui, key: &str, val: String) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
    let p = ui.painter();
    p.text(
        Pos2::new(r.left(), r.center().y),
        Align2::LEFT_CENTER,
        key,
        FontId::proportional(12.0),
        P::text_faint(),
    );
    p.text(
        Pos2::new(r.left() + 96.0, r.center().y),
        Align2::LEFT_CENTER,
        val,
        FontId::proportional(12.0),
        P::text(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use aox::llm::Usage;

    fn rec(round: u32, prompt: u64, cached: u64) -> UsageRecord {
        UsageRecord {
            round,
            at: 0,
            calls: 1,
            usage: Usage {
                prompt: Some(prompt),
                completion: Some(1),
                total: Some(prompt + 1),
                reasoning: None,
                cached: Some(cached),
                cache_miss: None,
            },
            last_prompt: Some(prompt),
            session_total: None,
            session_calls: 1,
        }
    }

    /// One finished call, the way the agent thread reports it.
    fn call(v: &mut CacheView, prompt: u64, cached: u64) {
        v.observe_call(
            &Usage { prompt: Some(prompt), cached: Some(cached), ..Default::default() },
            None,
        );
    }

    /// Run the strip's clock for `secs`, one 60 fps frame at a time, with a turn in flight.
    ///
    /// Frames rather than one call, because `advance` clamps a single frame to 100 ms on purpose: a
    /// stall must not teleport the front, so "two seconds" has to be two seconds *of frames*. And
    /// `busy` because the front only walks while work is in flight — see
    /// [`the_front_freezes_when_nothing_is_in_flight`].
    fn run(v: &mut CacheView, secs: f32) {
        v.gw.busy = true;
        let dt = 1.0 / 60.0;
        let mut t = 0.0;
        while t < secs - dt * 0.5 {
            v.advance(dt);
            t += dt;
        }
    }

    /// The strip's cadence, which is the one number the user supplied: one tile every 98 ms, the
    /// whole square in 25 seconds. Both are pinned here, and both are counted off the **clock** —
    /// that is the revision: tiles are seconds now, not calls.
    #[test]
    fn the_front_paints_a_tile_every_98_ms_and_a_lap_takes_25_seconds() {
        let mut v = CacheView::default();
        v.advance(5.0);
        assert_eq!(v.painted, 0, "an empty window sweeps over nothing at all");

        call(&mut v, 10_000, 9_800);
        assert_eq!(v.painted, 0, "a reading is not a tile: the front has to reach it");

        run(&mut v, 1.0);
        assert_eq!(v.painted, 10, "one second is 10.2 tiles");
        assert_eq!(v.front, 10);
        for i in 0..10 {
            assert_eq!(v.at(i).unwrap().verdict, Verdict::Hit, "tile {i} carries the reading");
        }
        assert!(v.at(10).is_none(), "and the next one does not exist yet");

        // The rest of the lap: 256 tiles, and the front comes back round to zero.
        while v.painted < BLOCKS {
            v.advance(1.0 / 60.0);
        }
        assert_eq!(v.front, 0, "a full lap puts the front back at the top");
        assert_eq!(v.painted, BLOCKS);
        for i in 0..BLOCKS {
            assert!(v.has(i), "every position has been reached");
        }

        // The next lap **overwrites** rather than resets: nothing goes blank again, and the tile the
        // front is about to paint is the one that has been waiting a whole lap.
        assert_eq!(v.behind(v.front), BLOCKS - 1);
        v.advance(1.0);
        assert_eq!(v.painted, BLOCKS, "the square stays full");
        assert!(v.at(0).is_some() && v.at(BLOCKS - 1).is_some());
    }

    /// The front walks only while work is in flight. An idle window, a question waiting for a choice
    /// and a plugin waiting for a permission are one case to this window — the next reading is
    /// blocked on a person — and the picture has to stop rather than scrubbing itself flat.
    #[test]
    fn the_front_freezes_when_nothing_is_in_flight() {
        let mut v = CacheView::default();
        call(&mut v, 10_000, 9_000);

        // Ten seconds of a window with nothing running: not one tile.
        v.gw.busy = false;
        for _ in 0..600 {
            v.advance(1.0 / 60.0);
        }
        assert_eq!((v.front, v.painted), (0, 0), "no work, no tiles");

        run(&mut v, 1.0);
        let (front, painted) = (v.front, v.painted);
        assert_eq!(painted, 10);

        // Work stops: the picture keeps every tile it had, reader included.
        v.gw.busy = false;
        for _ in 0..600 {
            v.advance(1.0 / 60.0);
        }
        assert_eq!((v.front, v.painted), (front, painted), "a stopped window keeps its picture");
        assert_eq!(v.now_cell().and_then(|i| v.at(i)).map(|r| r.seq), Some(1), "the reading stays");

        // And the stop is mid-tile, not a lost fraction: the tile that was half due lands as soon as
        // work resumes, without waiting a whole 98 ms on top of what it had already earned.
        let sweep = v.sweep;
        assert!(sweep > 0.0 && sweep < 1.0, "half a tile in hand: {sweep}");
        v.gw.busy = true;
        v.advance((1.0 - sweep + 0.001) / SWEEP_RATE);
        assert_eq!(v.painted, painted + 1, "the half-finished tile lands the moment work resumes");
    }

    /// The point of the revision: the front writes **the newest reading there is**, so a band's width
    /// is how long that reading stayed the newest thing known. Nothing replays old calls, and nothing
    /// has to agree with a second time axis.
    #[test]
    fn the_front_paints_the_newest_reading_and_a_band_is_its_duration() {
        let mut v = CacheView::default();
        call(&mut v, 10_000, 9_800); // hit
        run(&mut v, 2.0);
        let band = v.painted;
        assert!(band == 20 || band == 21, "two seconds is about twenty tiles: {band}");

        call(&mut v, 10_000, 200); // miss
        run(&mut v, 1.0);
        let n = v.painted;
        for i in 0..band {
            assert_eq!(v.at(i).unwrap().verdict, Verdict::Hit, "tile {i} predates the miss");
        }
        for i in band..n {
            assert_eq!(v.at(i).unwrap().verdict, Verdict::Miss, "tile {i} was painted after it");
        }
        let edges = (1..n)
            .filter(|i| v.at(*i).unwrap().verdict != v.at(*i - 1).unwrap().verdict)
            .count();
        assert_eq!(edges, 1, "exactly one edge, where the reading changed");
    }

    /// Fade is how the strip says "a while ago", and it is the *only* age encoding there is: full
    /// strength on the tile just written, the floor on the one the front is about to repaint.
    #[test]
    fn the_fade_runs_from_the_tile_just_written_to_the_one_due_next() {
        let mut v = CacheView::default();
        call(&mut v, 10_000, 9_000);
        run(&mut v, 1.0);
        assert_eq!(v.now_cell(), Some(9), "the tile just written");
        assert_eq!(v.fade(9), 1.0);
        assert!(v.fade(8) < 1.0 && v.fade(8) > 0.99, "one step back is barely dimmer");
        // On the first lap nothing is old yet: the oldest painted tile is only ten steps back.
        assert!(v.fade(0) > 0.95, "ten tiles in, nothing has faded far: {}", v.fade(0));

        while v.painted < BLOCKS {
            v.advance(1.0 / 60.0);
        }
        let due = v.front;
        assert_eq!(v.behind(due), BLOCKS - 1, "the tile due next has waited a whole lap");
        assert!(
            (v.fade(due) - (1.0 - FADE_MAX)).abs() < FADE_MAX / BLOCKS as f32 + 1e-6,
            "a full lap fades the next tile down to the floor: {}",
            v.fade(due)
        );
        assert!(v.fade(due) > 0.25, "the floor is a visible tile, not a hole");
    }

    /// "Now" is a position on the strip, not a call: the frame sits on the tile the front has just
    /// written, and moves one cell every 98 ms.
    #[test]
    fn the_frame_sits_on_the_tile_the_front_has_just_written() {
        let mut v = CacheView::default();
        assert_eq!(v.now_cell(), None, "no strip, no now");
        call(&mut v, 10_000, 9_000);
        assert_eq!(v.now_cell(), None, "still nothing until the front moves");

        run(&mut v, 1.0);
        assert_eq!(v.now_cell(), Some((v.front + BLOCKS - 1) % BLOCKS));
        let before = v.now_cell();
        run(&mut v, 1.0);
        assert_ne!(v.now_cell(), before, "and it does not stand still");
        assert_eq!(v.now_cell(), Some(v.front - 1), "always the tile just behind the front");
    }

    /// The headline must describe **the picture**: the strip's own weighted rate (hit 1, partial ½,
    /// miss 0), so a grid full of grey can never sit under "97%". The denominator is painted tiles,
    /// and a tile is 98 ms — so the figure is a share of time.
    #[test]
    fn the_headline_is_the_pictures_own_weighted_rate() {
        let mut v = CacheView::default();
        assert!(v.map_ratio().is_none(), "nothing painted, nothing claimed");

        call(&mut v, 1_000, 1_000); // hit
        run(&mut v, 1.0);
        call(&mut v, 1_000, 500); // partial: half credit
        run(&mut v, 1.0);
        let r = v.map_ratio().unwrap();
        assert!((r - 0.75).abs() < 1e-6, "ten hits then ten halves: {r}");
        assert_eq!(v.tally(), (10, 10, 0), "and the three rows of the card agree with it");

        // A miss after a quiet spell shows up as an *area* — that is the whole point of the model.
        call(&mut v, 1_000, 0);
        run(&mut v, 2.0);
        assert_eq!(v.tally(), (10, 10, 20), "two seconds of a miss is twenty tiles");
    }

    /// A turn's aggregate is a reading too, but a coarser one: it is used only when no per-call
    /// figure arrived this turn, and the tooltip says which it was.
    #[test]
    fn a_per_call_figure_wins_over_the_turn_aggregate() {
        let mut v = CacheView::default();
        v.observe_context(vec![Seg { label: "System prompt", tokens: 4_000 }]);
        call(&mut v, 4_000, 3_000);
        assert_eq!((v.used, v.hit), (4_000, 3_000));
        let reading = v.latest.unwrap();
        assert_eq!(reading.unit, Unit::Call);

        v.observe_turn(&rec(1, 9_000, 8_000));
        assert_eq!((v.used, v.hit), (4_000, 3_000), "the reading keeps the per-call figure");
        assert_eq!(v.latest.unwrap().seq, reading.seq, "…and the turn does not replace it");
        assert_eq!(v.saved, 8_000, "the lifetime figure comes from the aggregate");
        assert_eq!(v.prompt_total, 9_000);
        assert!(v.ratio().is_some_and(|r| (r - 0.75).abs() < 1e-6), "3K of 4K");

        // An endpoint that reports nothing per call still gets a reading — a coarser one, and the
        // tooltip calls it a round rather than a call.
        let mut w = CacheView::default();
        let mut r = rec(2, 8_000, 6_000);
        r.calls = 3;
        w.observe_turn(&r);
        assert_eq!((w.used, w.hit), (8_000, 6_000));
        let reading = w.latest.unwrap();
        assert_eq!(reading.unit, Unit::Round);
        assert_eq!(reading.calls, 3, "three calls behind one reading, and the tooltip says so");
        assert_eq!(reading.seq, 2, "round n");
    }

    /// Opening an old conversation cannot rebuild the strip: the file keeps one aggregate per turn
    /// and no timestamps, so there is no shape to recover — and inventing one would be exactly the
    /// decoration this window avoids. The newest aggregate becomes the reading the front writes, and
    /// the strip starts blank.
    #[test]
    fn replay_starts_the_strip_from_the_newest_round() {
        let mut v = CacheView::default();
        let log = [rec(1, 2_000, 0), rec(2, 4_000, 3_800), rec(3, 9_000, 5_000)];
        v.replay(&log);

        assert_eq!(v.painted, 0, "nothing is painted until the clock runs");
        let reading = v.latest.unwrap();
        assert_eq!((reading.seq, reading.unit), (3, Unit::Round));
        assert_eq!(reading.verdict, Verdict::Partial, "5K of 9K");

        // The lifetime totals come out of the same pass: three turns, summed.
        assert_eq!(v.prompt_total, 15_000);
        assert_eq!(v.saved, 8_800);
        assert_eq!(v.rounds, 3);

        // And painting starts from it, not from the first round.
        run(&mut v, 0.5);
        assert!(v.painted >= 5, "half a second of tiles");
        assert_eq!(v.at(0).unwrap().seq, 3, "the newest round is what the front writes");
    }

    /// Switching conversations must not leave the previous one's tiles on screen — the strip is a
    /// lens on *this* conversation, and the window itself stays open across the switch.
    #[test]
    fn a_new_conversation_starts_with_an_empty_strip() {
        let mut v = CacheView::default();
        call(&mut v, 10_000, 9_000);
        run(&mut v, 1.0);
        assert!(v.painted > 0);

        v.reset();
        assert_eq!((v.front, v.painted, v.calls), (0, 0, 0));
        assert!(v.at(0).is_none());
        assert!(v.latest.is_none(), "and no reading to paint with");
        assert!(v.map_ratio().is_none());
    }

    /// A missing `cached` field means "this endpoint never reports caching". The window must say
    /// that instead of printing a confident 0% — and must not write a tile, because a grey sea of
    /// misses would be an invention, not a reading.
    #[test]
    fn an_endpoint_without_cache_fields_is_flagged_and_writes_no_tile() {
        let mut v = CacheView::default();
        let mut r = rec(1, 1000, 0);
        r.usage.cached = None;
        v.observe_turn(&r);
        assert!(!v.has_cache, "no cache field has been seen");
        assert_eq!(v.hit, 0);
        assert_eq!(v.miss, 1000, "an unreported cache is a full miss, not a full hit");
        assert!(v.latest.is_none(), "but it is not a *measured* miss, so there is no reading");
        assert_eq!(v.painted, 0, "…and nothing to paint");
    }

    /// "Tokens saved" is the sum of the hits, so it can never exceed the prompt it was measured
    /// against — a saved figure above the spend would be nonsense on screen.
    #[test]
    fn lifetime_figures_stay_consistent() {
        let mut v = CacheView::default();
        for i in 1..=4u32 {
            v.observe_turn(&rec(i, 2000 * u64::from(i), 1500 * u64::from(i)));
        }
        assert_eq!(v.saved, 1500 + 3000 + 4500 + 6000);
        assert_eq!(v.prompt_total, 2000 + 4000 + 6000 + 8000);
        assert!(v.lifetime_ratio().is_some_and(|r| r > 0.0 && r < 1.0));
        assert_eq!(v.rounds, 4);
        // The reading reflects the **last** round, not the sum: 8000 tokens, 6000 of them cached.
        assert_eq!(v.used, 8000);
        assert_eq!(v.hit, 6000);
        assert_eq!(v.latest.unwrap().seq, 4, "and the last round is what the front will paint");
    }

    /// The default is the demo's own worst case: nothing observed yet, ratio `None` (not 0.0), and
    /// a window that says "no call observed" rather than "0% hit".
    #[test]
    fn before_any_call_there_is_no_ratio() {
        let v = CacheView::default();
        assert!(v.ratio().is_none());
        assert!(v.lifetime_ratio().is_none());
        assert!(v.map_ratio().is_none());
        assert_eq!(v.tally(), (0, 0, 0));
    }

    /// The in-flight estimate exists to make the *numbers* move while a call is out, and it must do
    /// exactly that and no more: widen the reading to the request being sent, never replace a
    /// measurement, never survive the reply that supersedes it — and never touch the strip, which is
    /// painted by the clock alone.
    #[test]
    fn the_estimate_widens_the_grid_and_a_measurement_replaces_it() {
        let mut v = CacheView::default();
        v.observe_turn(&rec(1, 10_000, 8_000));
        v.observe_context(vec![
            Seg { label: "System prompt", tokens: 12_000 },
            Seg { label: "Your input", tokens: 500 },
        ]);
        assert_eq!(v.width(), 12_500, "the estimate stands for the request in flight");
        assert_eq!(v.used, 10_000, "…without replacing the measurement");
        assert_eq!(v.painted, 0, "and without painting a tile");

        // The reply arrives: the estimate is dropped, not remembered.
        v.observe_call(
            &Usage { prompt: Some(12_400), cached: Some(11_000), ..Default::default() },
            None,
        );
        assert_eq!(v.width(), 12_400);
        assert_eq!((v.used, v.hit), (12_400, 11_000));
        assert_eq!(v.painted, 0, "a reading is not a tile: the clock paints");
        run(&mut v, 0.5);
        assert!(v.painted >= 5, "half a second of strip is five tiles");
        assert_eq!(v.at(0).unwrap().prompt, 12_400, "and it carries the measurement");

        // A later, smaller estimate must never shorten the reading below what was measured.
        v.observe_context(vec![Seg { label: "System prompt", tokens: 1_000 }]);
        assert_eq!(v.width(), 12_400, "the estimate may only ever add");
    }

    /// The band edges are the *only* invented thing in this window, so they get a test of their own:
    /// every state must be reachable from a measured share, and the edges must be inclusive on the
    /// side the legend implies.
    #[test]
    fn the_bands_map_a_measured_share_onto_the_three_states() {
        assert_eq!(Verdict::of(1.0), Verdict::Hit);
        assert_eq!(Verdict::of(HIT_MIN), Verdict::Hit, "the edge is a hit, not a partial");
        assert_eq!(Verdict::of(0.70), Verdict::Partial);
        assert_eq!(Verdict::of(0.20), Verdict::Partial);
        assert_eq!(Verdict::of(MISS_MAX), Verdict::Miss, "the edge is a miss");
        assert_eq!(Verdict::of(0.0), Verdict::Miss, "a cold start is a miss");
        // The weighting the headline uses: a partial is half a hit.
        assert_eq!(Verdict::Hit.score(), 1.0);
        assert_eq!(Verdict::Partial.score(), 0.5);
        assert_eq!(Verdict::Miss.score(), 0.0);
    }

    /// The walk is the whole window's shape: a curve that covers every cell exactly once and never
    /// jumps, so consecutive positions are neighbouring tiles. A jump would break the strip into
    /// pieces and destroy the one thing the curve is for — that a sequence of moments is a *line*
    /// laid into a square.
    #[test]
    fn the_walk_is_a_hilbert_curve_that_never_jumps() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..BLOCKS {
            let (r, c) = cell_of(i);
            assert!(r < GRID && c < GRID, "tile {i} falls outside the grid");
            assert!(seen.insert((r, c)), "tile {i} lands on a cell already visited");
            assert_eq!(flow_of(r, c), i, "flow_of is not the inverse of cell_of at {i}");
        }
        assert_eq!(seen.len(), BLOCKS as usize, "the walk must cover every cell");

        // Every step is a step: exactly one cell across or down.
        for i in 1..BLOCKS {
            let (pr, pc) = cell_of(i - 1);
            let (r, c) = cell_of(i);
            let d = (r as i32 - pr as i32).abs() + (c as i32 - pc as i32).abs();
            assert_eq!(d, 1, "tile {i} jumps from ({pr},{pc}) to ({r},{c})");
        }

        // …and it really is a Hilbert curve rather than anything else that happens to be
        // Hamiltonian: those four steps are its signature opening U, and a snake's are not.
        assert_eq!(
            [cell_of(0), cell_of(1), cell_of(2), cell_of(3)],
            [(0, 0), (0, 1), (1, 1), (1, 0)],
            "the curve must open with the Hilbert U from the top-left corner"
        );
        // Not row-major, not a snake: the last tile is nowhere near the bottom-right corner.
        assert_ne!(cell_of(BLOCKS - 1), (GRID - 1, GRID - 1));
        assert!(cell_of(16).0 < 4, "tile 16 is still near the top of the curve");
    }
}
