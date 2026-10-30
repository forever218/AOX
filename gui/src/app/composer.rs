//! The input card: the floating composer, its attachment row, the option slot, and the two
//! buttons that end a turn.
//!
//! The composer is an `Area` pinned by an explicit position, not a bottom panel (a floating card can be
//! scrolled past, which is what makes the transcript feel like a page). Its height is therefore
//! arithmetic rather than layout -- see `composer_base_h`, measured once per frame and reused by the
//! transcript's viewport in the same frame.

use super::*;

/// First-frame estimate for the floating input box's height, before it has been measured once.
///
/// This is **not** the transcript's reserve any more. The viewport's bottom edge is derived from the
/// same live height that positions the composer (`composer_top`), so the two can never disagree —
/// the fixed reserve could: it was 118 points while a measured card with a plan open was taller,
/// and the last lines of the conversation stayed underneath it.
pub(super) const COMPOSER_FIRST_FRAME_H: f32 = 118.0;

/// Gap between the composer card and the window's bottom edge.
pub(super) const COMPOSER_BOTTOM_GAP: f32 = 18.0;

/// How tall the input box may get before it starts **scrolling instead of growing**.
///
/// The card lives in an auto-height bottom panel, so an uncapped `TextEdit::multiline` grows the
/// panel until it swallows the window — a pasted document left the transcript with zero height
/// (2026-10-29). Seven rows is where the card stops reading as an input and starts reading as a
/// document; it is also still shorter than the transcript on a small window, so the conversation
/// never disappears entirely.
pub(super) const COMPOSER_MAX_ROWS: f32 = 7.0;

/// The height the input box takes **this** frame.
///
/// `measured` is the text's own height as reported by last frame's `ScrollArea::content_size`, or
/// `None` on the very first frame (before anything has been laid out).
///
/// Clamped to `[row_h, 7 rows]`: never shorter than one line (an empty box still has to look like a
/// box), never taller than the cap — past that the content scrolls instead. The lower clamp also
/// absorbs a stale small reading, which can happen on the frame right after the text is cleared.
pub(super) fn input_box_height(measured: Option<f32>, row_h: f32) -> f32 {
    measured.unwrap_or(row_h).clamp(row_h, row_h * COMPOSER_MAX_ROWS)
}

/// The example-question chip in the empty state.
/// One attachment chip (a cell in the pending-upload bar). Returns `true` if the user clicked the
/// × on the right.
///
/// The chip as a whole is **not clickable**; only the × is a hot zone: there is nowhere for this
/// chip to "open" to (an attachment is not a link), and making the whole thing clickable amounts
/// to "one click, one deletion", far too easy to fumble.
/// The last row of the option slot: **inline input** (answer with Enter or by clicking the arrow on
/// the right). Returns what the user typed.
///
/// The first version was just a "type your own..." button that handed focus to the big input box
/// below. The user's feedback (2026-10-06) was "don't use that big main input box below" -- the
/// place to answer should be beside the question, with no need for the eyes to run back and forth
/// between two boxes; and "after clicking the button you still have to go find another box" is
/// itself easily taken for "I clicked it and nothing happened".
///
/// Both Enter and the send button work: single-line input naturally wants Enter, but mouse users
/// shouldn't be forced to touch the keyboard.
pub(super) fn ask_input_row(ui: &mut Ui, w: f32, text: &mut String) -> Option<String> {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::surface());

    // The send button on the right (same drawing as the one on the main input box, one size down)
    let btn = Rect::from_center_size(
        Pos2::new(rect.right() - 19.0, rect.center().y),
        Vec2::splat(26.0),
    );
    // Leave the number column free on the left (12 padding + 26 column width) so the text is
    // left-aligned with the A / B / C above it
    let edit_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 38.0, rect.top() + 7.0),
        Pos2::new(btn.left() - 6.0, rect.bottom() - 7.0),
    );

    let resp = ui.put(
        edit_rect,
        TextEdit::singleline(text)
            .frame(false)
            .desired_width(edit_rect.width())
            .font(FontId::proportional(13.5))
            .hint_text(
                RichText::new(aox::i18n::t("You can also type here…")).color(P::quiet()).size(13.0),
            ),
    );
    // The standard test for "Enter" on a single-line TextEdit: it **lost focus** this frame and
    // Enter was pressed.
    // `key_pressed` alone won't do -- that would misfire on Enter while typing (it doesn't lose
    // focus).
    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

    let typed = text.trim().to_string();
    let can = !typed.is_empty();
    let bresp = ui.interact(btn, ui.id().with("aox-ask-send"), Sense::click());
    let hot = can && pointer_if(ui, &bresp);
    let p = ui.painter();
    if can {
        // Hover **lifts** the disc instead of dimming it. The old `gamma_multiply(0.75)` made the
        // button darker on hover, which on an ink disc reads as "pressed" — the opposite of what the
        // pointer is doing. A shadow 2 px down plus a half-pixel of growth is the same move the cards
        // make, so the button belongs to the same physical world (2026-10-29).
        let r = btn.width() * 0.5;
        if hot {
            p.circle_filled(btn.center() + egui::vec2(0.0, 2.0), r + 0.5, P::shade(0.16));
        }
        p.circle_filled(btn.center(), r + if hot { 0.6 } else { 0.0 }, P::ink());
    } else {
        // Draw a hollow circle while it's empty, so it's clear "there's a button here, you just have
        // to write something first"
        p.circle_stroke(btn.center(), btn.width() * 0.5, Stroke::new(1.0, P::border()));
    }
    paint_arrow_up(p, btn, if can { P::bg() } else { P::text_faint() });

    (can && (enter || bresp.clicked())).then_some(typed)
}

/// One option row in the option slot: `A` + the body text. Returns whether it was clicked.
pub(super) fn ask_row(ui: &mut Ui, w: f32, idx: usize, text: &str) -> bool {
    ask_chip(ui, w, idx, text)
}

/// One row in the option slot: `A` + the body text.
///
/// Hand-drawn + `allocate_exact_size(.., Sense::click())`, and **deliberately not `Frame::show`
/// followed by `interact()`**: at narrow widths the latter's hit zone gets squashed along with the
/// content (that is exactly what the first version did -- it drew as a 28px-wide sliver and
/// clicking it did nothing at all). Hand-drawn, the hit zone is the whole row: a definite width,
/// exactly coinciding with what is drawn.
///
/// The number column takes a fixed width, so the left edges of A / B / C's text **line up** -- the
/// option slot is a column, not a heap.
pub(super) fn ask_chip(ui: &mut Ui, w: f32, idx: usize, text: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let p = ui.painter();
    p.rect_filled(
        rect,
        CornerRadius::same(theme::R1),
        if resp.hovered() { P::surface_hi() } else { P::surface() },
    );
    // A stroke on hover, but `edge_raised` — a full-ink ring made the row look selected rather than
    // hovered, which is a different thing (the row is not selected until it is picked).
    if resp.hovered() {
        p.rect_stroke(
            rect,
            CornerRadius::same(theme::R1),
            Stroke::new(1.0, P::edge_raised()),
            egui::StrokeKind::Inside,
        );
    }

    const NUM_W: f32 = 26.0;
    let g = p.layout_no_wrap(
        aox::tool::ask::letter(idx).to_string(),
        FontId::proportional(13.5),
        P::text_faint(),
    );
    p.galley(
        Pos2::new(rect.left() + 12.0, rect.center().y - g.size().y * 0.5),
        g,
        P::text_faint(),
    );
    let color = P::text();
    // Estimate an upper bound from the available width and elide: splitting the difference between
    // ~13px for Han and ~7px for Latin gives 11px. Estimating loosely only leaves a bit too much or
    // too little blank -- **not eliding** would send a long option straight out of the row.
    let keep = (((w - NUM_W - 24.0) / 11.0).max(12.0)) as usize;
    let g = p.layout_no_wrap(middle_elide(text, keep), FontId::proportional(13.5), color);
    p.galley(
        Pos2::new(rect.left() + NUM_W + 8.0, rect.center().y - g.size().y * 0.5),
        g,
        color,
    );
    resp.clicked()
}

pub(super) fn attach_chip(ui: &mut Ui, name: &str) -> bool {
    let mut removed = false;
    Frame::NONE
        .fill(P::surface_hi())
        .corner_radius(CornerRadius::same(theme::pill(24.0)))
        .stroke(Stroke::new(1.0, P::edge()))
        .inner_margin(Margin::symmetric(11, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                ui.label(RichText::new(middle_elide(name, 30)).color(P::text_dim()).size(13.0));
                removed = close_x(ui);
            });
        });
    removed
}

/// Accumulate a conversation's totals turn by turn.
pub(super) fn sum_usage(log: &[UsageRecord]) -> Usage {
    let mut u = Usage::default();
    for r in log {
        u.accrue(&r.usage);
    }
    u
}

/// The text of a turn breakdown's row in the transcript.
///
/// "Lay every field out directly" is deliberate: this row is buried right next to the body text,
/// and whoever reads it is most likely checking the books -- making them open a second-level panel
/// is torment. Missing fields simply don't appear -- no padding out with 0s.
pub(super) fn usage_line(r: &UsageRecord) -> String {
    let mut parts = vec![aox::tf!("#{} · {} calls", r.round, r.calls)];
    let mut put = |k: &str, v: Option<u64>| {
        if let Some(v) = v {
            parts.push(format!("{k} {}", thousands(v)));
        }
    };
    put(aox::i18n::t("Input"), r.usage.prompt);
    put(aox::i18n::t("Output"), r.usage.completion);
    put(aox::i18n::t("Total"), r.usage.total);
    put(aox::i18n::t("Reasoning"), r.usage.reasoning);
    put(aox::i18n::t("Cache hit"), r.usage.cached);
    put(aox::i18n::t("Cache miss"), r.usage.cache_miss);
    parts.join(" · ")
}

/// The token breakdown line in the transcript. Deliberately lighter than the body text: it is a
/// footnote, not content, so it isn't even given a bullet -- a row of numbers says what it is on
/// its own.
pub(super) fn usage_row(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(P::text_faint()).size(12.5));
    ui.add_space(10.0);
}

/// The breakdown unfolded from the "Σ total tokens" chip: **this conversation's totals**.
///
/// The turn-by-turn bills are not here -- each of those rows sits with its own AI reply in the
/// transcript (see `usage_line`). What this answers is a different question: "how much did this
/// whole conversation cost, and where did it go".
pub(super) fn usage_detail(ui: &mut Ui, log: &[UsageRecord]) {
    // Tighten the line spacing. This is "a column of numbers", and the default item spacing
    // stretches it out into something like a loose card, making it harder to see at a glance which
    // rows belong to the same entry.
    ui.spacing_mut().item_spacing.y = 1.0;
    let rows: Vec<&UsageRecord> = log.iter().filter(|r| !r.is_empty()).collect();
    if rows.is_empty() {
        ui.label(
            RichText::new(aox::i18n::t("No usage records for this conversation yet"))
                .color(P::text_faint())
                .size(13.0),
        );
        return;
    }
    let mut sum = Usage::default();
    for r in &rows {
        sum.accrue(&r.usage);
    }
    let calls: u32 = rows.iter().map(|r| r.calls).sum();

    /// One "label ... number" row. A `v` of None skips the row entirely.
    ///
    /// Allocate this row's rect directly rather than using `horizontal` + `with_layout` -- the
    /// latter stretches the row height to the content area's leftover in a vertical container, so a
    /// column of numbers comes out as loose as an empty card.
    /// The numbers use a monospace face so that right-aligned they line up vertically; reading the
    /// numbers matters more than reading the labels.
    fn row(ui: &mut Ui, k: &str, v: Option<u64>) {
        let Some(v) = v else { return };
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 21.0), Sense::hover());
        let p = ui.painter();
        p.text(
            Pos2::new(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            k,
            FontId::proportional(13.0),
            P::text_faint(),
        );
        p.text(
            Pos2::new(rect.right(), rect.center().y),
            Align2::RIGHT_CENTER,
            thousands(v),
            theme::mono_font(13.5),
            P::text(),
        );
    }

    ui.label(
        RichText::new(aox::tf!("This conversation · {} rounds · {} calls", rows.len(), calls))
            .color(P::text())
            .size(13.5),
    );
    ui.add_space(6.0);

    row(ui, aox::i18n::t("Input"), sum.prompt);
    row(ui, aox::i18n::t("Output"), sum.completion);
    row(ui, aox::i18n::t("Total"), sum.total);
    // The sub-items are indented one level under the "totals": reasoning is part of output and
    // cache is part of input
    row(ui, aox::i18n::t("  of which reasoning"), sum.reasoning);
    row(ui, aox::i18n::t("  cache hit"), sum.cached);
    row(ui, aox::i18n::t("  cache miss"), sum.cache_miss);

    // Last input != total input: the former is "how big is the current context", the latter is
    // "how much was read in across all these turns". Showing both is what keeps "the context is
    // nearly full" from being misread as "one turn was too expensive".
    if let Some(v) = rows.last().and_then(|r| r.last_prompt) {
        let (rule, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
        ui.painter()
            .hline(rule.x_range(), rule.center().y, Stroke::new(1.0, P::border()));
        ui.add_space(9.0);
        row(ui, aox::i18n::t("Current context"), Some(v));
    }
}

impl App {
    /// Take the files dropped into the window this frame.
    ///
    /// **Anywhere in the window accepts them** (explicit user request, 2026-10-05) -- hence
    /// reading from `ctx` rather than hanging it off a widget: hanging it off a widget means
    /// only that one small hot zone works, and dropping anywhere else does nothing at all.
    ///
    /// egui reports "files dropped this frame" once per frame (`dropped_files`); reading it
    /// drains it, so nothing is double-counted. `hovered_files` is what brightens the input
    /// card's border by one step.
    pub(super) fn take_dropped(&mut self, ctx: &egui::Context) {
        let (dropped, hovering) = ctx.input(|i| (i.raw.dropped_files.clone(), !i.raw.hovered_files.is_empty()));
        self.dragging_files = hovering;
        if dropped.is_empty() {
            return;
        }
        for f in dropped {
            // Ones with no local path (some platforms drag in-memory data) have to be given up --
            // attachments are read by path
            let Some(path) = f.path else {
                self.items.push(Item::Note(
                    NoteKind::Warn,
                    aox::tf!("Dropped \"{}\" has no local path and cannot be used as an attachment.", f.name),
                ));
                continue;
            };
            match aox::attach::Attachment::probe(&path) {
                // The same file dropped twice = one attachment (don't show two identical chips)
                Ok(a) if self.pending.iter().any(|p| p.path == a.path) => {}
                Ok(a) => self.pending.push(a),
                // State the real reason it was rejected (directory / unreadable / image over
                // 32 MiB), don't just say "failed"
                Err(e) => self.items.push(Item::Note(NoteKind::Warn, e)),
            }
        }
    }
    /// The row of clickable options above the input box (asked by the model via `ask_user`).
    ///
    /// Same place and same language as the pending-upload bar (small rounded blocks + a thin
    /// stroke), because they answer the same class of question: "what is the user about to give
    /// me". The only difference is that one gives files and the other gives a decision.
    ///
    /// **Vertical**: A / B / C ... one per row, and the last one is always "type your own".
    /// The first version was horizontal, and one run on real hardware gave it away: with many
    /// options the last one got squashed into a 28px-wide sliver (one character per line) and
    /// **clicking it did nothing** -- the hit zone deformed along with the squashed content, so
    /// what was drawn and what could be clicked no longer matched (the user reported both
    /// problems together on 2026-10-06).
    /// Vertical also settles "read it in one sweep": you read an option and pick it, whereas
    /// horizontal makes you hunt left and right.
    pub(super) fn choice_bar(&mut self, ui: &mut Ui) {
        // Clone rather than drawing through `self.choice`: the last row needs
        // `&mut self.choice_text`, which together with "read the question and options" is two
        // borrows of the same self -- the struct is tiny, so cloning is the least trouble.
        let Some(card) = self.choice.clone() else { return };
        let mut picked: Option<usize> = None;
        let mut typed: Option<String> = None;

        ui.add_space(6.0);
        // The question the model is waiting on: a **raised** card, unlike everything else in the
        // transcript. It is the one block that wants an answer, so it is allowed to sit above the
        // page instead of in it — that is what the shadow and the brighter edge are saying.
        theme::floating(theme::card_radius())
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                let w = ui.available_width();
                ui.set_width(w);
                // The question is copied verbatim: rewording it means it is no longer what the
                // model asked, and that is precisely what the user has to answer
                ui.label(RichText::new(&card.question).color(P::text()).size(14.5));
                ui.add_space(9.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    for (i, o) in card.options.iter().enumerate() {
                        if ask_row(ui, w, i, o) {
                            picked = Some(i);
                        }
                    }
                    // The last row is **the input box itself** (see `ask_input_row`): the place to
                    // answer is right beside the question, so the eyes don't have to run back and
                    // forth between two boxes.
                    typed = ask_input_row(ui, w, &mut self.choice_text);
                });
            });
        ui.add_space(6.0);

        if let Some(i) = picked {
            self.answer_choice(ChoiceReply::Picked(i));
        } else if let Some(t) = typed {
            self.choice_text.clear();
            self.answer_choice(ChoiceReply::Text(t));
        }
    }
    /// The pending-upload bar -- the row of small chips **above** the input card, one per
    /// attachment.
    ///
    /// Over-long names are **elided in the middle** (`a very long name...shot.png`): the
    /// extension is the single most useful bit here (one glance tells you image or document), so
    /// cut the middle and keep the tail. Paths are never shown -- an `E:\...\...\...` prefix
    /// would blow the chip apart, and the user knows where they dragged it from.
    pub(super) fn attach_bar(&mut self, ui: &mut Ui) {
        if self.pending.is_empty() {
            return;
        }
        let mut remove: Option<usize> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(7.0, 7.0);
            for (i, a) in self.pending.iter().enumerate() {
                if attach_chip(ui, &a.name) {
                    remove = Some(i);
                }
            }
        });

        // When an image is attached and the current model probably has no vision, give a hint
        // that **can be ignored**.
        //
        // Deliberately not blocked: what the user wants is "send it anyway, let the endpoint
        // error and the model relay that" (see the `attach` module docs). This sentence only
        // moves "you'll mysteriously get a 400 in a while" forward to "you can see it right
        // now".
        if self.pending.iter().any(|a| a.kind.is_some()) && !self.model.to_ascii_lowercase().contains("flash") {
            ui.add_space(5.0);
            ui.label(
                RichText::new(aox::tf!(
                    "Current model {}: on the DeepSeek side only deepseek-flash supports image input (sending it anyway is fine; the endpoint will report an error).",
                    self.model
                ))
                .color(P::warn())
                .size(12.5),
            );
        }
        // Deletion waits until the loop is done: `iter()` still borrows `self.pending`
        if let Some(i) = remove {
            self.pending.remove(i);
        }
        ui.add_space(8.0);
    }

    pub(super) fn send(&mut self) {
        if self.demo {
            return;
        }
        let text = self.input.trim().to_string();

        // ── an option slot is up: this sentence is the **answer** to that question, not a new
        // turn ──
        //
        // Getting this wrong is hard to debug: the user types and hits send while the agent
        // thread is blocked in `choose()` -- the `Input` just queues up in the channel and it
        // looks like "I typed and it ignored me, and the option slot is still sitting there".
        // After answering, the input box clears and the option slot folds away, and that turn
        // carries on.
        if self.choice.is_some() {
            if !text.is_empty() {
                self.input.clear();
                self.answer_choice(ChoiceReply::Text(text));
            }
            return;
        }

        // Attachments with no text still count as a message -- dragging an image in and hitting
        // enter is the most common usage.
        if (text.is_empty() && self.pending.is_empty()) || self.busy || self.waiting_auth {
            return;
        }
        if self.boot.as_ref().is_some_and(|b| !b.provider_ready) {
            return;
        }
        let files: Vec<PathBuf> = self.pending.iter().map(|a| a.path.clone()).collect();
        let names: Vec<String> = self.pending.iter().map(|a| a.name.clone()).collect();
        // The timeline records only "how many attachments were sent": that is a glanceable
        // ledger, not a file listing (the real record of attachments is in the conversation
        // file, see the path list in `attach::to_content`).
        let label = if names.is_empty() {
            text.clone()
        } else if text.is_empty() {
            aox::tf!("({} attachments)", names.len())
        } else {
            aox::tf!("{} ({} attachments)", text, names.len())
        };
        self.items.push(Item::User(text.clone(), names));
        // Images dragged in by the user are shown **on the spot**, without waiting for the
        // host's acknowledgement to make a round trip.
        let shown: Vec<PathBuf> = files.iter().filter(|p| looks_like_image(p)).cloned().collect();
        if !shown.is_empty() {
            self.items.push(Item::Images(shown));
        }
        self.input.clear();
        self.pending.clear();
        self.force_follow = true;
        // Open a turn: the top "input" row on the timeline starts from this moment
        activity::lock(&self.act).round_begin(&label, self.now());
        let _ = self.tx.send(ToAgent::Input(text, files));
    }

    pub(super) fn stop(&mut self) {
        if self.busy {
            self.flags.request_abort();
            self.status = aox::i18n::t("Interrupting…").into();
        }
    }

    /// Hand the user's answer back to the agent thread and fold the option slot away.
    ///
    /// This is the **only** place that sends a `ChoiceReply`: all three answering paths
    /// (clicking an option, typing and sending, and any hotkey added later) go through it, so
    /// there can be no "one path forgot the reply and the model waits forever".
    pub(super) fn answer_choice(&mut self, reply: ChoiceReply) {
        if let Some(tx) = self.choice_reply.take() {
            let _ = tx.send(reply);
        }
        self.choice = None;
        self.choice_text.clear();
        // The answer was just given and that turn is still running (the model has more to do),
        // so the state goes back to "thinking" rather than staying on "waiting for you" and
        // making it look like it is still waiting.
        self.status = aox::i18n::t("Thinking").into();
    }

    /// Finishing and queueing image decoding: once per frame.
    ///
    /// It lives here rather than "at the moment an Item is created" for two reasons:
    /// (1) Items come from two sources (history replay and live events) and
    ///     `items_from_history` is a free function with no `self`, so queueing on both sides
    ///     would mean writing the same thing twice;
    /// (2) one sweep is **idempotent** -- a path that already has an entry isn't queued again,
    ///     so no convention like "remember to call this after step X" is needed.
    /// Collect the image paths that still need a texture, deduplicated.
    ///
    /// Kept outside `poll_images` so the three sources listed there share one rule for "is this
    /// worth decoding"; a second copy is how one of them ends up showing "Decoding image…" forever.
    pub(super) fn queue_images(want: &mut Vec<PathBuf>, tex: &HashMap<PathBuf, Tex>, paths: &[PathBuf]) {
        for p in paths {
            if looks_like_image(p) && !tex.contains_key(p) && !want.contains(p) {
                want.push(p.clone());
            }
        }
    }

    pub(super) fn poll_images(&mut self, ctx: &egui::Context) {
        // ① Collect: finished decodes become textures (`try_recv` doesn't block the UI)
        let done: Vec<(PathBuf, Result<aox::image::Bitmap, String>)> = self
            .tex
            .iter_mut()
            .filter_map(|(p, t)| match t {
                Tex::Loading(rx) => rx.try_recv().ok().map(|r| (p.clone(), r)),
                _ => None,
            })
            .collect();
        for (p, r) in done {
            let entry = match r {
                // Upload parameters live in one place, `crate::tex` (which also spells out why
                // getting them wrong doesn't raise an error)
                Ok(bm) => Tex::Ready(crate::tex::upload(
                    ctx,
                    // The texture name uses only the file name: it is a debugging label, and a
                    // whole path is long and full of backslashes
                    format!("aox-img:{}", p.file_name().unwrap_or_default().to_string_lossy()),
                    &bm,
                )),
                // Failures are cached too: otherwise every frame would spin up another decode
                // thread for the same broken image
                Err(e) => Tex::Failed(e),
            };
            self.tex.insert(p, entry);
        }

        // ② Queue: ones not yet queued go into the queue.
        //
        // Three sources: a user's own attachments (`Item::Images`), images produced by tools and
        // already sitting under their call row (`Item::Process`), and the same thing while the turn
        // is still running (`Live::steps`).
        let mut want: Vec<PathBuf> = Vec::new();
        for it in &self.items {
            match it {
                Item::Images(paths) => Self::queue_images(&mut want, &self.tex, paths),
                Item::Process(p) => {
                    for st in &p.steps {
                        Self::queue_images(&mut want, &self.tex, &st.images);
                    }
                }
                _ => {}
            }
        }
        if let Some(l) = &self.live {
            for st in &l.steps {
                Self::queue_images(&mut want, &self.tex, &st.images);
            }
        }
        // At most 4 decode threads per frame: a screen with twenty images shouldn't spin up
        // twenty threads at once, and even less should it unfurl twenty decoded bitmaps
        // simultaneously (that can run to gigabytes). The rest are queued on the next frame --
        // which costs nothing visually, since the user only looks at one or two at a time anyway.
        let queued = want.len();
        for p in want.into_iter().take(4) {
            let (tx, rx) = std::sync::mpsc::channel();
            let path = p.clone();
            // Decoding is synchronous and can take tens of milliseconds (GDI+, see `aox::image`),
            // so it must never occupy the UI thread
            std::thread::spawn(move || {
                let _ = tx.send(aox::image::decode(&path));
            });
            self.tex.insert(p, Tex::Loading(rx));
        }

        // ③ Anything still decoding or not yet queued asks for another frame -- otherwise, once
        //    the window goes quiet those images sit on "decoding..." forever (egui only repaints
        //    when there is input).
        if queued > 0 || self.tex.values().any(|t| matches!(t, Tex::Loading(_))) {
            ctx.request_repaint();
        }
    }

    /// The token count in the input box's bottom-right corner: **click** to unfold the breakdown
    /// above it.
    ///
    /// Click, not hover, and that is a direct consequence of the move: the chip now sits in the
    /// corner the pointer crosses on its way to the send button, so a hover-opened panel would
    /// flash open every time someone sent a message. The panel stays up until the chip is clicked
    /// again, or until something else is clicked (see the dismiss test below).
    ///
    /// The number is **live**: completed turns are exact, the running turn is the stream's own
    /// estimate, so the total climbs during a turn instead of sitting still until it ends. The
    /// `~` marks the estimate half.
    pub(super) fn usage_chip(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let base = self.usage_sum().total.unwrap_or(0);
        let est = self.live.as_ref().map_or(0, |l| l.out.tokens());
        let shown = base + est;
        // Nothing has been spent and nothing is being spent: no chip at all, rather than a
        // "Σ 0 tok" that would read as a measurement.
        if shown == 0 && !self.busy {
            return;
        }
        let text = if est > 0 {
            format!("Σ ~{} tok", thousands(shown))
        } else {
            format!("Σ {} tok", thousands(shown))
        };
        // Drawn by hand rather than as a `Label`: the hover plate has to go **under** the text, and
        // a widget's own paint has already happened by the time its response says "hovered". The
        // same reason the drawer's icons allocate a rect and then paint into it.
        //
        // The width follows the number, which is why the ring is added *before* this in the
        // right-to-left row: the chip grows to the left and the ring keeps its place in the corner.
        let galley = ui
            .painter()
            .layout_no_wrap(text.clone(), FontId::proportional(13.0), P::text_dim());
        let (rect, resp) =
            ui.allocate_exact_size(galley.size() + egui::vec2(12.0, 6.0), Sense::click());
        let hov = pointer_if(ui, &resp);
        let lit = hov || self.usage_open;
        if lit {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(theme::R0), P::hover_fill());
        }
        let fg = if lit { P::ink() } else { P::text_dim() };
        let galley = ui
            .painter()
            .layout_no_wrap(text, FontId::proportional(13.0), fg);
        ui.painter()
            .galley(rect.min + egui::vec2(6.0, 3.0), galley, fg);
        if resp.clicked() {
            self.usage_open = !self.usage_open;
        }
        if !self.usage_open {
            return;
        }
        // The panel unfolds **upwards**: the chip is in the window's bottom-right corner, so
        // "below it" is off-screen. `pivot(LEFT_BOTTOM)` puts the panel's own bottom-left corner
        // on the anchor, which is what makes it grow up instead of down.
        //
        // ⚠️ **Do not convert coordinates here**: `Area::fixed_pos` and `ui.allocate_*` are both in
        // **client-area local coordinates** (`Area::begin` uses `ctx.content_rect()` as the
        // constraint). This once had a "viewport origin" term, `viewport().inner_rect.left_top()`,
        // added -- that is **screen (monitor) coordinates**, a completely different system, and the
        // conversion is meaningless; the result was the panel shifting wholesale with the
        // window's position on the monitor (measured ~(154, 14) logical pixels off when the
        // window wasn't at (0,0)). The same mistake was made in `workplace_ui::menu` and has been
        // fixed there too.
        let anchor = resp.rect.left_top() - egui::vec2(0.0, 6.0);
        let log = &self.usage_log;
        let panel_rect = egui::Area::new(egui::Id::new("aox-usage-panel"))
            .fixed_pos(anchor)
            .pivot(Align2::LEFT_BOTTOM)
            .order(egui::Order::Foreground)
            .show(&ctx, |ui| {
                Frame::popup(ui.style())
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(theme::R2))
                    .stroke(Stroke::new(1.0, P::edge_raised()))
                    .inner_margin(Margin::symmetric(16, 13))
                    .show(ui, |ui| usage_detail(ui, log))
                    .response
                    .rect
            })
            .inner;
        // Dismissed by a click anywhere else. Tested after the panel has been drawn, so the click
        // that opens it (which lands on the chip) can't immediately close it again.
        let inside = resp.hovered() || panel_rect.contains(ui.ctx().pointer_hover_pos().unwrap_or_default());
        if ctx.input(|i| i.pointer.any_click()) && !inside {
            self.usage_open = false;
        }
    }

    /// The **totals** across this conversation's turns (summed field by field).
    ///
    /// Both the corner chip and the expanded panel use it -- one number says "how much was
    /// spent", the whole set says "where it went".
    pub(super) fn usage_sum(&self) -> Usage {
        sum_usage(&self.usage_log)
    }

    /// How much of the model's context window this conversation currently occupies -- the ring's
    /// numerator.
    ///
    /// `last_prompt` on the newest record is exactly that number: it is the size of the **last
    /// request**, i.e. everything the model was handed (system prompt + the whole history + the new
    /// message). The running turn's output is added on top, because it is about to become part of
    /// the next request -- without it the ring would sit perfectly still while a long reply is
    /// being written, which is the one moment someone is likely to be watching it.
    pub(super) fn context_used(&self) -> u64 {
        let live = self.live.as_ref().map_or(0, |l| l.out.tokens());
        // Right after a compaction the true figure is only known locally (the compacted request's
        // size); the endpoint's own number arrives with the next call. Showing the stale "full"
        // reading in between is exactly the moment the user is watching, so the reset wins until a
        // real number lands (see `ctx_reset`).
        if let Some(after) = self.ctx_reset {
            return after + live;
        }
        let base = self
            .usage_log
            .iter()
            .rev()
            .find_map(|r| r.last_prompt)
            .unwrap_or(0);
        base + live
    }

    /// Put each turn's breakdown from the ledger back into the transcript, after that turn's AI
    /// reply.
    ///
    /// Location relies on the natural boundary "every user message starts a turn": turn N's
    /// breakdown falls between the Nth user message and the next one (or at the end if there
    /// isn't one). That way, opening an old conversation puts every turn's bill back where it was,
    /// instead of heaping them all at the top.
    pub(super) fn rebuild_usage_rows(&mut self) {
        self.items.retain(|it| !matches!(it, Item::Usage(_)));
        if self.usage_log.is_empty() {
            return;
        }
        // First work out the end index of each turn's content
        let mut ends: Vec<usize> = Vec::new();
        for (i, it) in self.items.iter().enumerate() {
            if matches!(it, Item::User(..)) {
                if let Some(prev) = ends.len().checked_sub(1) {
                    ends[prev] = i.saturating_sub(1);
                }
                ends.push(i);
            }
        }
        if let Some(last) = ends.last_mut() {
            *last = self.items.len().saturating_sub(1);
        }
        // Insert in reverse: later turns first, so earlier turns' indexes don't get skewed
        for rec in self.usage_log.iter().rev() {
            let Some(&end) = ends.get(rec.round.saturating_sub(1) as usize) else {
                continue;
            };
            self.items.insert(end + 1, Item::Usage(usage_line(rec)));
        }
    }

    pub(super) fn composer(&mut self, ui: &mut Ui) {
        let (inner, pad) = column(ui.available_width());
        let blocked = self.demo || self.boot.as_ref().is_some_and(|b| !b.provider_ready);
        // While an option slot is up, **the meaning of send changes**: it means "this is my
        // answer" rather than starting a new turn (that turn is running, so `busy` is true). So it
        // has to be let through here -- otherwise the user is looking at a row of options, wants
        // to type their own clearer answer, and finds the send key greyed out, leaving only
        // options that all miss the point.
        let choosing = self.choice.is_some();
        // Attachments with no text may be sent too (dragging an image in and pressing send is the
        // most common usage).
        // This does not apply while answering: then you really have to type; a mere attachment is
        // not an answer.
        let can_send = !blocked
            && !self.waiting_auth
            && (!self.busy || choosing)
            && (!self.input.trim().is_empty()
                || (!choosing && !self.pending.is_empty()));

        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.vertical(|ui| {
                ui.set_width(inner);
                ui.set_max_width(inner);

                // The option slot: a multiple-choice question the model asked via `ask_user`. It
                // goes **above** the pending-upload bar -- it is more urgent than "what am I sending
                // this time" (until the user answers, this turn doesn't move).
                self.choice_bar(ui);

                // The pending-upload bar: the row of small chips **above** the input card (the
                // position the user explicitly asked for on 2026-10-05).
                self.attach_bar(ui);

                // The floating input box: no separator line and no fill around it, floating above
                // the transcript on a large corner radius + a thin stroke + a faint shadow (which is
                // where the "hollowed-out background" look comes from).
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(theme::R3))
                    .stroke(Stroke::new(
                        if self.dragging_files { 1.5 } else { 1.0 },
                        // While files are being dragged over the window, brighten the stroke one step:
                        // where it will land on release has to be visible
                        if self.dragging_files {
                            P::ink()
                        } else if self.busy || self.waiting_auth {
                            P::edge_raised()
                        } else {
                            P::edge()
                        },
                    ))
                    .shadow(theme::card_shadow())
                    .inner_margin(Margin::symmetric(22, 15))
                    .show(ui, |ui| {
                        let w = ui.available_width();
                        ui.set_width(w);
                        ui.set_max_width(w);
                        // The plan lives **inside** the input card, above a hairline divider: as a
                        // card of its own it left a seam against the box, and a card floating over
                        // the transcript hid the last line of the conversation behind it (both
                        // reported 2026-10-23).
                        self.plan_bar(ui);
                        // Note: ui.horizontal_centered() can't be used here -- it requests "the full
                        // available size" in one go, which makes the auto-height bottom bar panel keep
                        // inflating (measured: it swallows the whole window). ui.horizontal centres on
                        // the cross axis just the same, but its initial height is a single row, so
                        // there is no feedback loop.
                        ui.horizontal(|ui| {
                            let hint = if self.demo {
                                aox::i18n::t("Demo mode: layout preview only (start with --demo; run normally to chat)")
                            } else if blocked {
                                aox::i18n::t("Model not configured; chat unavailable")
                            } else if self.waiting_auth {
                                aox::i18n::t("Waiting for authorization…")
                            } else if choosing {
                                // Point at the inline input box **on the row above**: the place to
                                // answer is beside the question, and people shouldn't be directed to the
                                // box below (explicit user request, 2026-10-06)
                                aox::i18n::t("Click one of the options above, or type in that row…")
                            } else if self.busy {
                                aox::i18n::t("Running; click Stop on the right")
                            } else {
                                aox::i18n::t("Tell AOX what to do…")
                            };
                            // Explicitly subtract the send button's width so the input box can't push
                            // the button out of the card
                            let edit_w = (w - 62.0).max(120.0);
                            let text_edit = TextEdit::multiline(&mut self.input)
                                .desired_rows(1)
                                .desired_width(edit_w)
                                .frame(false)
                                .hint_text(RichText::new(hint).color(P::quiet()).size(16.0));
                            // ── Height cap: grows to seven rows, then scrolls ──
                            //
                            // A bare `TextEdit::multiline` grows with its content, and the input card
                            // sits in an **auto-height** bottom panel — so pasting a few dozen lines
                            // made the panel eat the whole window and squeeze the transcript to
                            // nothing (reported 2026-10-29).
                            //
                            // `desired_rows(1)` only sets the *minimum*; the cap has to come from
                            // outside. Measured alternatives (2026-10-30, inside `ui.horizontal`):
                            //
                            // | form                                   | height with 1 / 5 / 12 lines |
                            // |----------------------------------------|------------------------------|
                            // | `ScrollArea::max_height(h)`            | 64 / **64** / **64**         |
                            // | `+ auto_shrink([false, true])`         | 64 / **64** / **64**         |
                            // | `+ min_scrolled_height(1.0)`           | 30 / **30** / **30**         |
                            // | `allocate_ui_with_layout` + that h     | 64 / 94 / 128 (capped)       |
                            //
                            // So inside a *horizontal* layout the scroll area's own auto-shrink does
                            // not track the content — it pins itself to whatever height it measured
                            // first, which is exactly the "height is now fixed, it no longer grows"
                            // report. Allocating the box explicitly is the only form that follows.
                            //
                            // The box height comes from the **previous frame's** `content_size` (the
                            // natural height of the text), clamped to [one row, seven rows]. That
                            // costs one frame of lag while the content changes; the alternative was
                            // hard-coding TextEdit's internal minimum (measured 64 pt at a 16 pt
                            // body, ~3.5 rows), which would drift the moment the font size changed.
                            let row_h = ui.text_style_height(&egui::TextStyle::Body);
                            let hid = ui.id().with("composer-input-h");
                            let prev_h = ui
                                .ctx()
                                .data(|d| d.get_temp::<f32>(hid))
                                .unwrap_or(row_h);
                            let box_h = input_box_height(Some(prev_h), row_h);
                            let area = ui.allocate_ui_with_layout(
                                egui::vec2(edit_w, box_h),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_width(edit_w);
                                    ui.set_max_width(edit_w);
                                    egui::ScrollArea::vertical()
                                        // Stable id: rebuilding it every frame would reset the
                                        // scroll offset on every keystroke.
                                        .id_salt("aox-composer-input")
                                        .max_height(box_h)
                                        .show(ui, |ui| ui.add(text_edit))
                                },
                            );
                            // Record the natural height for the *next* frame to size the box with.
                            // Taken before the scroll area clamped it, so a long document still
                            // reports "this much text" rather than "this much viewport".
                            ui.ctx()
                                .data_mut(|d| d.insert_temp(hid, area.inner.content_size.y));
                            let resp = area.inner.inner;
                            if self.focus_input {
                                resp.request_focus();
                                self.focus_input = false;
                            }

                            let ime =
                                ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
                            let enter = ui.input(|i| {
                                i.key_pressed(egui::Key::Enter) && !i.modifiers.shift && !i.modifiers.alt
                            });
                            if resp.has_focus() && enter && !ime && can_send {
                                self.send();
                            }

                            ui.add_space(8.0);
                            if self.busy {
                                if stop_button(ui).clicked() {
                                    self.stop();
                                }
                            } else if send_button(ui, can_send).clicked() && can_send {
                                self.send();
                            }
                        });

                        // Second row: the workplace button at the bottom-left, the usage readouts
                        // at the bottom-right.
                        //
                        // On its own row rather than squeezed into the previous one: it says "under
                        // what permissions this conversation runs", which is a different thing from
                        // "type + send"; squeezing them together narrows the input box, and this
                        // button sitting right next to the round send button also invites misclicks
                        // (one merely "sends a message", the other "drops all restrictions").
                        //
                        // In demo mode it **can be clicked** (`self.demo || ...`): the list /
                        // confirmation / directory picker it opens are pure UI, so simulating them in
                        // place is enough to show the layout (see `send_ws`).
                        //
                        // The right-hand pair is the "what has this cost" corner: the token total
                        // (click → the breakdown unfolds upwards) and the context ring next to it.
                        // They moved here from the top bar because the top bar is where you look
                        // when you want to **act**, and this is where the hand already is when you
                        // want to **know**.
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            // The model chip comes **first**, before the workplace chip: it answers
                            // "who is answering", which is the more fundamental of the two questions
                            // this row asks, and the eye starts at the left.
                            //
                            // It is clickable even when the model is not configured (`!blocked` is
                            // false then): on a fresh install this chip *is* the way to configure it,
                            // so greying it out would leave the one reachable control dead.
                            let model_clicked = crate::model_ui::chip(ui, &mut self.mdl, true);
                            if model_clicked {
                                // One pop-up list at a time: the two panels anchor to the same row
                                // and would sit on top of each other.
                                self.ws.close_menu();
                            }
                            ui.add_space(2.0);
                            let ws_clicked = workplace_ui::button(ui, &mut self.ws, self.demo || !blocked);
                            if ws_clicked {
                                self.mdl.close_menu();
                            }
                            // The recovery chip: one step to the right of the workspace button,
                            // because both answer "what can this conversation do to the tree".
                            //
                            // Hidden entirely when recovery is off, and in `full` mode — there is no
                            // protected workspace there, so there is nothing to roll back and a chip
                            // would only promise something that cannot happen.
                            let show_rec = self.boot.as_ref().is_some_and(|b| b.recovery)
                                && !matches!(
                                    self.ws.view().map(|v| &v.mode),
                                    Some(aox::workplace::Mode::Full)
                                );
                            if show_rec {
                                ui.add_space(2.0);
                                if let Some(req) = self.rec.chip(ui, true, self.busy) {
                                    self.send_recovery(req);
                                }
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                // While the host is folding old turns into a summary, the gauge has no
                                // honest reading (the numbers only come back a round trip later), so
                                // the ring becomes the spinner and a line of text says why. That is
                                // the **only** moment the input box stops looking idle while a turn
                                // is still running.
                                let spin = self.compacting.map(|t| t.elapsed().as_secs_f32());
                                context_ring(ui, self.context_used(), self.context_window, spin);
                                if spin.is_some() {
                                    ui.add_space(6.0);
                                    ui.label(
                                        RichText::new(aox::i18n::t("Compacting context…"))
                                            .size(11.0)
                                            .color(P::text_dim()),
                                    );
                                    // Nothing else in this row animates, so the repaint has to be
                                    // asked for by hand; without it the arc would jump only when the
                                    // pointer moves.
                                    ui.ctx().request_repaint();
                                }
                                ui.add_space(10.0);
                                self.usage_chip(ui);
                            });
                        });
                    });
            });
        });
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_box_grows_until_seven_rows_then_scrolls() {
        let row_h = 20.0;
        // First frame: nothing measured yet, so exactly one row.
        assert_eq!(input_box_height(None, row_h), row_h);
        // A short document: the box follows the text.
        assert_eq!(input_box_height(Some(60.0), row_h), 60.0);
        // Exactly at the cap: still the cap.
        assert_eq!(input_box_height(Some(row_h * 7.0), row_h), row_h * 7.0);
        // Past the cap: the box stops growing and the rest scrolls.
        assert_eq!(input_box_height(Some(row_h * 40.0), row_h), row_h * 7.0);
        // A stale tiny reading (text just cleared) cannot collapse the box.
        assert_eq!(input_box_height(Some(1.0), row_h), row_h);
    }
}
