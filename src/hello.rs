//! The **one line on the empty first screen** — where it comes from, and how it is cleaned up.
//!
//! Three sources, one shape. The GUI picks which one (a preference), this module answers with a
//! short piece of plain text:
//!
//! * **custom** — the user's own sentence. Nothing to do here; the GUI already holds it.
//! * **hitokoto** — one `GET` against a plain-text quoting API (default
//!   [`HITOKOTO_DEFAULT`], documented at <https://developer.hitokoto.cn/sentence/>). The URL is
//!   user-editable, including one on this machine, which is why it goes through
//!   [`crate::llm::endpoint_client`] rather than the `web_fetch` egress policy: the address was
//!   typed by hand, so it sits at the same trust level as the model endpoint — and `web_fetch`'s
//!   "no private addresses" rule would silently break a self-hosted service on `127.0.0.1`.
//! * **model** — one tiny chat call with **no tools** and a prompt built from the local clock and
//!   (optionally) the title of the most recent conversation.
//!
//! # What this module refuses to do
//!
//! - **Never invent a line that reads like UI copy.** Every fallback is an ordinary sentence a
//!   person could say out loud; the point of the feature is warmth, and "Loading…" is the opposite.
//! - **Never throw the reason away.** The GUI shows the failure only in the settings panel, never on
//!   the first screen — a greeting that apologises for itself is worse than the local fallback.
//! - **Never take longer than it must.** The fetch is bounded by [`FETCH_TIMEOUT`]; the greeting
//!   sits on the startup path, where "slow" and "broken" have to look different quickly.
//!
//! # Privacy
//!
//! Only the model source sends anything, and it sends exactly two things: the local time string and
//! the most recent conversation **title** (not its content). The settings panel says so in one line.

use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::llm::LlmProvider;
use crate::message::Message;

/// The quoting API's default address: `c=f` is "a random line from a curated pool", `encode=text`
/// asks for plain text instead of JSON — that is the whole reason this feature needs no JSON parse.
pub const HITOKOTO_DEFAULT: &str = "https://v1.hitokoto.cn/?c=f&encode=text";

/// The developer documentation offered next to the address field.
pub const HITOKOTO_DOCS: &str = "https://developer.hitokoto.cn/sentence/";

/// Hard ceiling for one fetch. Long enough for a slow but working service, short enough that the
/// first screen does not sit blank while the user wonders whether the feature is on.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// Longest line accepted (in `char`s). Beyond this it stops being a greeting and becomes a paragraph
/// on the first screen; the surplus is cut and an ellipsis added, so the cut is visible rather than
/// mistaken for the API having returned a short line.
pub const MAX_LINE: usize = 60;

/// One source's answer, as the GUI consumes it.
///
/// A plain `Result` would do, but the GUI has to tell "the network said no" (worth a line in the
/// settings panel) from "the model is not configured" (already reported elsewhere) — so the reason
/// travels as text either way.
pub enum Line {
    Text(String),
    Failed(String),
}

// ─────────────────────────── the clock ───────────────────────────

/// The local hour, `0..24`. Straight from the OS through the one implementation the project
/// already has (`history::local_now`), so the greeting and the dated memory note never disagree
/// about what time it is.
pub fn hour() -> u16 {
    crate::history::local_now().hour
}

/// A **vague** label for this moment — the only thing the model is told about the clock.
///
/// Deliberately coarse, and it never names the weekday: told "Saturday 14:20", a model reliably
/// writes "Happy Saturday afternoon!" no matter what style it was given. Reducing the clock to a
/// mood ("lazy weekend afternoon") keeps the same information at the resolution a person would
/// actually bring up, and leaves room for the line to be about something else entirely.
fn time_label(t: &crate::history::Local) -> &'static str {
    // Fully qualified on purpose: `t` is already the parameter name here, and importing the
    // translation function would shadow it.
    let weekend = matches!(t.weekday, 0 | 6);
    match (t.hour, weekend) {
        (5..=8, _) => crate::i18n::t("early morning"),
        (9..=10, _) => crate::i18n::t("morning"),
        (11..=13, _) => crate::i18n::t("lunch time"),
        (14..=17, true) => crate::i18n::t("lazy weekend afternoon"),
        (14..=17, false) => crate::i18n::t("afternoon"),
        (18..=20, _) => crate::i18n::t("evening, dinner time"),
        (21..=23, _) => crate::i18n::t("late evening"),
        _ => crate::i18n::t("deep night"),
    }
}

// ─────────────────────────── the local pool ───────────────────────────

/// Which part of the day it is — the only thing the local lines vary on.
///
/// Five buckets rather than "morning / afternoon / evening": the two edges (still awake at 03:00,
/// eating at 12:30) are exactly the moments where the right line lands and a generic one does not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Daypart {
    LateNight,
    Morning,
    Noon,
    Afternoon,
    Evening,
}

impl Daypart {
    pub fn of(hour: u16) -> Self {
        match hour {
            0..=4 => Daypart::LateNight,
            5..=10 => Daypart::Morning,
            11..=13 => Daypart::Noon,
            14..=17 => Daypart::Afternoon,
            _ => Daypart::Evening,
        }
    }
}

/// The local fallback: used when the chosen source cannot answer in time.
///
/// Written to be sayable — short, warm, occasionally a little silly, never a status message. `index`
/// exists so the caller can vary the line (two new conversations in a row should not open with the
/// same sentence) without this module owning any clock or RNG.
pub fn fallback_line(part: Daypart, index: usize) -> String {
    let pool: [&str; 3] = match part {
        Daypart::LateNight => [
            crate::i18n::t("Still up? Let's keep it small."),
            crate::i18n::t("Quiet hours. Good time for one small thing."),
            crate::i18n::t("The night is calm — what are we making?"),
        ],
        Daypart::Morning => [
            crate::i18n::t("Morning! What are we building?"),
            crate::i18n::t("Good morning — where do we start?"),
            crate::i18n::t("Morning. Coffee first, or code first?"),
        ],
        Daypart::Noon => [
            crate::i18n::t("Lunch time — go eat something."),
            crate::i18n::t("Noon. A short break, then we pick it up."),
            crate::i18n::t("Have you eaten yet?"),
        ],
        Daypart::Afternoon => [
            crate::i18n::t("Afternoon. Shall we continue?"),
            crate::i18n::t("Good afternoon — what's next?"),
            crate::i18n::t("The slow hours. Perfect for tidying something up."),
        ],
        Daypart::Evening => [
            crate::i18n::t("A conversation under the night sky."),
            crate::i18n::t("Evening. Ready when you are."),
            crate::i18n::t("Winding down, or just getting started?"),
        ],
    };
    pool[index % pool.len()].to_string()
}

/// A line for "the source failed and we have no clock opinion" — the settings panel's preview and
/// the very first frame both use it.
pub fn ready_line() -> String {
    crate::i18n::t("Ready when you are.").to_string()
}

// ─────────────────────────── cleaning ───────────────────────────

/// Turn whatever came back into **one plain line**, or nothing.
///
/// Everything hostile about remote text is handled here rather than at the call site, because there
/// are two callers (the API and the model) and they fail differently: an API may return HTML, a
/// model loves to wrap its answer in quotes and add a sentence of commentary. Rules, in order:
///
/// 1. strip a BOM (some services emit one before the first character);
/// 2. keep the **first non-empty line** — a model that ignores "one line only" is common, and the
///    second line is always commentary;
/// 3. collapse every run of whitespace/tabs into a single space;
/// 4. drop matching wrapping quotes (`"` `'` `“”` `「」`) — a quoted greeting renders as a quote;
/// 5. cut at [`MAX_LINE`] chars with an ellipsis.
pub fn sanitize(raw: &str) -> Option<String> {
    let raw = raw.trim_start_matches('\u{feff}');
    let first = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?
        // The API may answer with HTML when it is unhappy; a tag is not a greeting.
        .to_string();
    let mut line = String::with_capacity(first.len());
    let mut last_space = false;
    for ch in first.chars() {
        if ch.is_whitespace() {
            if last_space || line.is_empty() {
                continue;
            }
            last_space = true;
            line.push(' ');
        } else {
            last_space = false;
            line.push(ch);
        }
    }
    let line = trim_wrapping_quotes(line.trim());
    if line.is_empty() {
        return None;
    }
    let mut out: String = line.chars().take(MAX_LINE).collect();
    if line.chars().count() > MAX_LINE {
        out.push('…');
    }
    Some(out)
}

/// Remove one pair of matching quotes around the whole string, if there is one.
fn trim_wrapping_quotes(s: &str) -> &str {
    const PAIRS: [(char, char); 5] =
        [('"', '"'), ('\'', '\''), ('“', '”'), ('「', '」'), ('『', '』')];
    let mut chars = s.chars();
    let (Some(first), Some(last)) = (chars.next(), s.chars().last()) else { return s };
    for (open, close) in PAIRS {
        if first == open && last == close && s.chars().count() >= 2 {
            let start = first.len_utf8();
            let end = s.len() - last.len_utf8();
            return s[start..end].trim();
        }
    }
    s
}

// ─────────────────────────── the network ───────────────────────────

/// Ask the quoting API for one line.
///
/// The body **is** the line (`encode=text`), so there is nothing to parse; what comes back still
/// goes through [`sanitize`], because "plain text" from a proxy error page is plain text too.
pub async fn hitokoto(url: &str) -> Result<String> {
    let url = url.trim();
    if url.is_empty() {
        return Err(anyhow!(crate::i18n::t("no address")));
    }
    let client = crate::llm::endpoint_client(url).timeout(FETCH_TIMEOUT).build()?;
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        // The status is the useful half: 429 and 404 mean different fixes.
        return Err(anyhow!(crate::tf!("HTTP {}", status.as_u16())));
    }
    sanitize(&body).ok_or_else(|| anyhow!(crate::i18n::t("the answer was empty")))
}

// ─────────────────────────── variety ───────────────────────────

/// The writing styles the model is asked to follow — **one per call**.
///
/// Variety comes from code rather than from asking the model to "be varied": a model told only
/// "write a greeting" converges on the same two sentences all day, and each extra instruction it
/// invents for itself ("say good morning!") is one more thing every launch has to live with. Naming
/// one style per call makes the spread real, and extending it is one line here.
///
/// The literals are the English wording **and** the catalogue keys; [`generate`] translates the one
/// it drew, so a Chinese build hands the model a Chinese style instruction.
const STYLES: &[&str] = &[
    "a short noun phrase naming the moment, like a scene title",
    "a quiet, cozy murmur",
    "a light invitation, with no question mark",
    "a tiny observation about everyday life",
    "a gentle mood label, just two or three words",
    "something about food, drink, or a small daily ritual",
    "a half-finished thought that invites the user to continue",
];

/// The system prompt. One line of output, and the list of things that make a line feel generated
/// (greetings, questions, "what do you want to talk about") is **closed off by name**: telling a
/// model what not to do is worth more than asking it to "be natural".
///
/// The output language is stated explicitly, and **each catalogue states its own** — `t()` swaps
/// the whole paragraph, so the English build says "English" and the Chinese build says 中文. Relying
/// on "the model will answer in the language of the prompt" is not enough: the model's training data
/// is full of English one-liners for empty screens, and without this line a Chinese build reliably
/// came back with "Good afternoon".
const MODEL_PROMPT: &str = concat!(
    "You write the short line shown on an empty chat screen, like a tiny scene ",
    "title or a murmur. It is not a question and not a status message. ",
    "Warm, a little playful, everyday.\n",
    "Length: 2 to 8 words, at most 40 characters.\n",
    "Range of tone (never copy these): Lunch time / Evening chat / ",
    "Looks like rain outside / Take it slow today / Coffee first, then talk / ",
    "Anything goes\n",
    "Rules:\n",
    "- Never start with Hi, Hello, Hey, or Welcome.\n",
    "- Avoid the pattern \"What do you want to talk about\" and avoid question marks.\n",
    "- If the message gives a local time, you may greet with it (\"Saturday afternoon\") ",
    "or let it colour the line; never print the date or the clock digits.\n",
    "- If the message says not to refer to the time, do not mention the hour or the day.\n",
    "- Follow the style instruction in the user message.\n",
    "- Write the line in English.\n",
    "Output the line only: no quotes, no emoji, no explanation.",
);

/// A tiny dependency-free RNG.
///
/// The same trade the GUI's background picker makes (`brand::roll`): this module needs "one of
/// seven" and "three in ten", not a distribution library, and `rand` would be paid for out of the
/// binary-size budget the project tracks. xorshift64*, seeded from the clock and a call counter —
/// the counter matters because `SystemTime` on Windows is coarse enough that two calls in the same
/// millisecond would otherwise pick the same style.
struct Picker(u64);

/// Scramble a seed before xorshift touches it (splitmix64's finalizer).
///
/// xorshift is **linear**: two seeds a few hundred milliseconds apart — which is exactly what two
/// launches in a row look like — walk out of it to nearby states, and `n % 7` then lands in the same
/// bucket often enough to be noticed ("it gave me the same line twice"). A finalizer is what breaks
/// that: neighbouring inputs come out as unrelated outputs.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Picker {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let seed =
            nanos ^ CALLS.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let seed = mix(seed);
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..n` (`0` when the range is empty).
    fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        (self.next() >> 33) as usize % n
    }
}

/// The weekday's name, for the precise-time draw.
///
/// Translated: a Chinese build has to hand the model 星期六, not "Saturday" — an English token in the
/// middle of an otherwise Chinese message is exactly what pulls the answer back into English.
fn weekday_name(index: u16) -> &'static str {
    const NAMES: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    crate::i18n::t(NAMES[(index as usize) % 7])
}

/// `Saturday, 15:20` — the day and the hour, and **no date**.
///
/// The weekday is what makes "Saturday afternoon" possible, and the hour is the "precise time" this
/// draw exists for. The date is left out on purpose: a model handed `2026-10-31` prints it back.
fn exact_time(t: &crate::history::Local) -> String {
    format!("{}, {:02}:{:02}", weekday_name(t.weekday), t.hour, t.minute)
}

/// What the model is told about "now", one draw per call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clock {
    /// The precise reading: day and hour. The line may greet with it ("Saturday afternoon").
    Exact,
    /// A vague mood ("lazy weekend afternoon").
    Mood,
    /// Nothing at all — the line has to stand on its own.
    None,
}

/// How often each kind of clock comes up, in percent of calls.
///
/// The exact reading gets the largest share because it is the one that makes a line feel written for
/// *this* moment; the mood is its quieter version, and the empty draw is what keeps the feature from
/// turning into a clock that recites the time every launch.
const CLOCK_EXACT: usize = 45;
const CLOCK_MOOD: usize = 35;

fn pick_clock(pick: &mut Picker) -> Clock {
    match pick.below(100) {
        n if n < CLOCK_EXACT => Clock::Exact,
        n if n < CLOCK_EXACT + CLOCK_MOOD => Clock::Mood,
        _ => Clock::None,
    }
}

/// What one call tells the model about this moment.
///
/// Split out from [`generate`] so the prompt can be inspected without a network: which of the three
/// clock draws is in play changes the wording a lot, and that is easier to pin down as a pure
/// function of its inputs.
fn user_prompt(style: &str, clock: Clock, local: &crate::history::Local) -> String {
    let mut user = crate::tf!("Style for this one: {}", style);
    match clock {
        Clock::Exact => user.push_str(&crate::tf!("\nLocal time: {}", exact_time(local))),
        Clock::Mood => user.push_str(&crate::tf!(
            "\nTime of day (a loose mood hint, not something to state): {}",
            time_label(local)
        )),
        // Saying it explicitly beats silence: without this line the model tends to assume evening,
        // because that is what its training data's "greeting" examples look like.
        Clock::None => user.push_str(crate::i18n::t("\nDo not refer to the time of day.")),
    }
    user
}

/// Ask the model for one line.
///
/// Everything variable about the request — the style, and which of the three clock draws is used —
/// is decided here, in code, by [`Picker`]. The model is handed one instruction and no choices; that
/// is what keeps two launches from producing the same sentence.
///
/// **Nothing from the conversation is sent.** An earlier version passed the last conversation's
/// title as optional inspiration; it was removed on 2026-10-31 because a greeting that refers to
/// yesterday's work reads as surveillance, not as a welcome.
pub async fn generate(llm: &dyn LlmProvider) -> Result<String> {
    let mut pick = Picker::new();
    // The drawn style is translated here, not inside `STYLES`: the catalogue keys have to stay the
    // English literals, so the lookup belongs at the point of use.
    let style = crate::i18n::t(STYLES[pick.below(STYLES.len())]);
    let clock = pick_clock(&mut pick);
    let local = crate::history::local_now();

    let messages = [
        Message::system(crate::i18n::t(MODEL_PROMPT)),
        Message::user(user_prompt(style, clock, &local)),
    ];
    let reply = llm.chat(&messages, &[], None).await?;
    let text = reply.message.text().unwrap_or_default().to_string();
    sanitize(&text).ok_or_else(|| anyhow!(crate::i18n::t("the model said nothing usable")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quoted / multi-line / padded answer has to come out as exactly one clean line. These are
    /// the three shapes a model actually produces, and each of them looks broken on the first
    /// screen if it leaks through.
    #[test]
    fn sanitize_keeps_one_clean_line() {
        assert_eq!(sanitize("  \"Morning!\"  \n\nSure, here you go.").as_deref(), Some("Morning!"));
        assert_eq!(sanitize("「夜色下的对话」").as_deref(), Some("夜色下的对话"));
        assert_eq!(sanitize("\u{feff}Ready").as_deref(), Some("Ready"));
        // Internal runs collapse, but a single space survives (CJK sentences use none anyway)
        assert_eq!(sanitize("a   b\t\tc").as_deref(), Some("a b c"));
        assert_eq!(sanitize("   \n\t\n"), None, "whitespace is not a greeting");
        assert_eq!(sanitize(""), None);
        // A single quote mark is not a wrapping pair
        assert_eq!(sanitize("\"").as_deref(), Some("\""));
    }

    /// Over-long text is cut **and marked**, so "the API returned something short" and "we cut it"
    /// do not look the same.
    #[test]
    fn sanitize_cuts_long_lines_visibly() {
        let long = "好".repeat(MAX_LINE + 10);
        let line = sanitize(&long).unwrap();
        assert_eq!(line.chars().count(), MAX_LINE + 1, "60 chars plus the ellipsis");
        assert!(line.ends_with('…'));
        // Exactly at the limit: nothing is added
        let exact = sanitize(&"好".repeat(MAX_LINE)).unwrap();
        assert_eq!(exact.chars().count(), MAX_LINE);
        assert!(!exact.ends_with('…'));
    }

    /// Every bucket has to answer, and the answer must not be empty — a fallback that returns ""
    /// means the first screen is blank exactly when the network is down.
    #[test]
    fn every_daypart_has_lines() {
        for h in 0..24u16 {
            let part = Daypart::of(h);
            for i in 0..3 {
                let line = fallback_line(part, i);
                assert!(!line.trim().is_empty(), "hour {h} bucket {part:?} index {i} is empty");
            }
        }
        // The edges are the point of the five buckets
        assert_eq!(Daypart::of(3), Daypart::LateNight);
        assert_eq!(Daypart::of(8), Daypart::Morning);
        assert_eq!(Daypart::of(12), Daypart::Noon);
        assert_eq!(Daypart::of(16), Daypart::Afternoon);
        assert_eq!(Daypart::of(21), Daypart::Evening);
        assert_eq!(Daypart::of(0), Daypart::LateNight);
        assert_eq!(Daypart::of(23), Daypart::Evening);
    }

    /// The weekday index is the one thing the greeting reads off `Local`, so it has to be right on
    /// known dates (and on both leap-day paths).
    #[test]
    fn weekday_index_is_correct() {
        assert_eq!(crate::history::weekday_of(2026, 10, 31), 6, "2026-10-31 is a Saturday");
        assert_eq!(crate::history::weekday_of(2000, 1, 1), 6, "2000-01-01 is a Saturday");
        assert_eq!(crate::history::weekday_of(2024, 2, 29), 4, "2024-02-29 is a Thursday");
        assert_eq!(crate::history::weekday_of(2026, 1, 1), 4, "2026-01-01 is a Thursday");
    }

    /// The time is reduced to a **mood**, and never to a weekday or a clock reading. This is the
    /// property the whole "no 'Happy Saturday afternoon'" rule rests on.
    #[test]
    fn the_clock_becomes_a_mood_not_a_timestamp() {
        let local = |hour: u16, weekday: u16| crate::history::Local {
            year: 2026,
            month: 10,
            day: 31,
            weekday,
            hour,
            minute: 45,
            second: 0,
        };
        assert_eq!(time_label(&local(3, 3)), "deep night");
        assert_eq!(time_label(&local(7, 3)), "early morning");
        assert_eq!(time_label(&local(10, 3)), "morning");
        assert_eq!(time_label(&local(12, 3)), "lunch time");
        assert_eq!(time_label(&local(15, 3)), "afternoon");
        assert_eq!(time_label(&local(15, 6)), "lazy weekend afternoon");
        assert_eq!(time_label(&local(19, 0)), "evening, dinner time");
        assert_eq!(time_label(&local(22, 3)), "late evening");
        // No label ever carries a weekday, a date, or a number.
        for h in 0..24u16 {
            for wd in 0..7u16 {
                let label = time_label(&local(h, wd));
                assert!(!label.chars().any(|c| c.is_ascii_digit()), "{label} has a number");
                for day in ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"] {
                    assert!(!label.contains(day), "{label} names a weekday");
                }
            }
        }
    }

    /// The prompt hands over **one** style, and the style list has to stay a list of styles — an
    /// empty or duplicated entry would quietly collapse the variety the feature is built on.
    #[test]
    fn every_style_is_usable() {
        assert!(STYLES.len() >= 5);
        for s in STYLES {
            assert!(s.len() > 10, "a style has to describe something: {s:?}");
            assert!(!s.ends_with('.'), "styles are fragments, not sentences: {s:?}");
        }
        let mut uniq: Vec<&str> = STYLES.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), STYLES.len(), "two styles are the same, so one is never used");
    }

    /// The user message never names a weekday, and it says **something** about the clock either way:
    /// including a mood, or explicitly telling the model not to mention the time.
    #[test]
    fn the_three_clock_draws_say_different_things() {
        let local = crate::history::Local {
            year: 2026,
            month: 10,
            day: 31,
            weekday: 6,
            hour: 15,
            minute: 20,
            second: 0,
        };

        // The precise draw: the day and the hour, and **no date** (a model handed a date prints it).
        let exact = user_prompt("a quiet murmur", Clock::Exact, &local);
        assert!(exact.contains("Style for this one: a quiet murmur"), "{exact}");
        assert!(exact.contains("Local time: Saturday, 15:20"), "{exact}");
        assert!(!exact.contains("2026"), "the date must not reach the model: {exact}");

        // The mood draw: a vague label, never a weekday.
        let mood = user_prompt("a mood label", Clock::Mood, &local);
        assert!(mood.contains("lazy weekend afternoon"), "{mood}");
        assert!(!mood.contains("Saturday"), "{mood}");

        // The empty draw: explicitly told not to bring up the time.
        let none = user_prompt("a scene title", Clock::None, &local);
        assert!(none.contains("Do not refer to the time of day"), "{none}");
        assert!(!none.contains("Saturday"), "{none}");
        assert!(!none.contains("15:20"), "{none}");
    }

    /// All three draws have to be reachable from one picker — a weighted match that never reaches
    /// its last arm would quietly mean "the clock is always mentioned".
    #[test]
    fn every_clock_draw_can_come_up() {
        let mut p = Picker(0x0BAD_C0DE_1234_5678);
        let (mut exact, mut mood, mut none) = (0, 0, 0);
        for _ in 0..300 {
            match pick_clock(&mut p) {
                Clock::Exact => exact += 1,
                Clock::Mood => mood += 1,
                Clock::None => none += 1,
            }
        }
        assert!(exact > 60 && mood > 40 && none > 20, "{exact} / {mood} / {none}");
        assert_eq!(exact + mood + none, 300);
    }

    /// The exact draw is the common one, and the two weights have to stay a partition of 100 —
    /// otherwise `pick_clock`'s last arm silently absorbs the difference.
    #[test]
    fn the_clock_weights_are_the_ones_documented() {
        assert_eq!(CLOCK_EXACT, 45);
        assert_eq!(CLOCK_MOOD, 35);
        assert!(CLOCK_EXACT + CLOCK_MOOD < 100);
    }

    /// The picker has to stay in range and actually vary — a seed that collapses to a constant would
    /// look like it works while every call gets the same style.
    #[test]
    fn the_picker_stays_in_range_and_varies() {
        let mut p = Picker(0x1234_5678_9ABC_DEF0);
        let picks: Vec<usize> = (0..64).map(|_| p.below(STYLES.len())).collect();
        assert!(picks.iter().all(|&i| i < STYLES.len()));
        let mut uniq = picks.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert!(uniq.len() > 1, "64 draws from 7 styles all landed on {picks:?}");
    }

    /// The two addresses the UI offers must be the documented ones; a typo here sends every user to
    /// a 404 they cannot diagnose.
    #[test]
    fn the_documented_addresses_are_the_defaults() {
        assert_eq!(HITOKOTO_DEFAULT, "https://v1.hitokoto.cn/?c=f&encode=text");
        assert_eq!(HITOKOTO_DOCS, "https://developer.hitokoto.cn/sentence/");
    }

    /// **Two launches in a row must not produce the same style.**
    ///
    /// The seeds one launch apart differ by a few hundred milliseconds — neighbouring numbers, which
    /// is exactly the shape a linear generator (xorshift) maps to neighbouring states. Measured
    /// before [`mix`] existed: three runs in a row drew the same style *and* the same clock branch,
    /// which reads as "the feature is stuck". Seven styles across 64 nearby seeds is a low bar that a
    /// raw seed fails and a scrambled one clears by a wide margin.
    #[test]
    fn nearby_seeds_do_not_all_land_on_one_style() {
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..64u64 {
            let mut p = Picker(mix(1_700_000_000_000_000_000 + i * 1_000_000));
            seen.insert(p.below(STYLES.len()));
        }
        assert_eq!(seen.len(), STYLES.len(), "only got {seen:?} out of 7 styles");
    }
}
