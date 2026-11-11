//! The input card: the floating composer, its attachment row, the option slot, and the two buttons that end a turn.
//! The composer is an `Area` pinned by an explicit position, not a bottom panel (a floating card can be scrolled...

use super::*;

/// First-frame estimate for the floating input box's height, before it has been measured once.
pub(super) const COMPOSER_FIRST_FRAME_H: f32 = 118.0;

/// Gap between the composer card and the window's bottom edge.
pub(super) const COMPOSER_BOTTOM_GAP: f32 = 18.0;

/// How tall the input box may get before it starts scrolling instead of growing.
pub(super) const COMPOSER_MAX_ROWS: f32 = 7.0;

/// The height the input box takes this frame.
pub(super) fn input_box_height(measured: Option<f32>, row_h: f32) -> f32 {
    measured.unwrap_or(row_h).clamp(row_h, row_h * COMPOSER_MAX_ROWS)
}

/// The example-question chip in the empty state.
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
    // The standard test for "Enter" on a single-line TextEdit: it lost focus this frame and Enter was pressed....
    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

    let typed = text.trim().to_string();
    let can = !typed.is_empty();
    let bresp = ui.interact(btn, ui.id().with("aox-ask-send"), Sense::click());
    let hot = can && pointer_if(ui, &bresp);
    let p = ui.painter();
    if can {
        // Hover lifts the disc instead of dimming it.
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
    // Estimate an upper bound from the available width and elide: splitting the difference between ~13px for Han...
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

/// The token breakdown line in the transcript.
pub(super) fn usage_row(ui: &mut Ui, text: &str, back: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        // Right-to-left so the chip owns a fixed slot at the right end and the numbers get whatever is left: the usage...
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if back {
                clicked = jump_chip(ui);
            } else {
                ui.allocate_space(Vec2::new(JUMP_CHIP + 4.0, JUMP_CHIP));
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label(RichText::new(text).color(P::text_faint()).size(12.5));
            });
        });
    });
    ui.add_space(10.0);
    clicked
}

/// The breakdown unfolded from the "Σ total tokens" chip: this conversation's totals.
pub(super) fn usage_detail(ui: &mut Ui, log: &[UsageRecord]) {
    // Tighten the line spacing. This is "a column of numbers", and the default item spacing stretches it out...
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

    /// One "label... number" row.
    /// Allocate this row's rect directly rather than using `horizontal` + `with_layout` -- the latter stretches...
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

    // Last input != total input: the former is "how big is the current context", the latter is "how much was read...
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
    pub(super) fn choice_bar(&mut self, ui: &mut Ui) {
        // Clone rather than drawing through `self.choice`: the last row needs `&mut self.choice_text`, which together...
        let Some(card) = self.choice.clone() else { return };
        let mut picked: Option<usize> = None;
        let mut typed: Option<String> = None;

        ui.add_space(6.0);
        // The question the model is waiting on: a raised card, unlike everything else in the transcript.
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
                    // The last row is the input box itself (see `ask_input_row`): the place to answer is right beside the question...
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
    /// The pending-upload bar -- the row of small chips above the input card, one per attachment.
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

        // When an image is attached and the current model probably has no vision, give a hint that can be ignored.
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

        // ── an option slot is up: this sentence is the answer to that question, not a new turn ──.
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
        // The timeline records only "how many attachments were sent": that is a glanceable ledger, not a file listing...
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
    pub(super) fn answer_choice(&mut self, reply: ChoiceReply) {
        if let Some(tx) = self.choice_reply.take() {
            let _ = tx.send(reply);
        }
        self.choice = None;
        self.choice_text.clear();
        // The answer was just given and that turn is still running (the model has more to do), so the state goes back...
        self.status = aox::i18n::t("Thinking").into();
    }

    /// Finishing and queueing image decoding: once per frame.
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

        // Queue: ones not yet queued go into the queue.
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
        // At most 4 decode threads per frame: a screen with twenty images shouldn't spin up twenty threads at once...
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

        // Anything still decoding or not yet queued asks for another frame -- otherwise, once the window goes quiet those...
        if queued > 0 || self.tex.values().any(|t| matches!(t, Tex::Loading(_))) {
            ctx.request_repaint();
        }
    }

    /// The token count in the input box's bottom-right corner: click to unfold the breakdown above it.
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
        // Drawn by hand rather than as a `Label`: the hover plate has to go under the text, and a widget's own paint has...
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
        // The panel unfolds upwards: the chip is in the window's bottom-right corner, so "below it" is off-screen....
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

    /// The totals across this conversation's turns (summed field by field).
    pub(super) fn usage_sum(&self) -> Usage {
        sum_usage(&self.usage_log)
    }

    /// How much of the model's context window this conversation currently occupies -- the ring's numerator.
    pub(super) fn context_used(&self) -> u64 {
        let live = self.live.as_ref().map_or(0, |l| l.out.tokens());
        // Right after a compaction the true figure is only known locally (the compacted request's size); the endpoint's...
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

    /// Put each turn's breakdown from the ledger back into the transcript, after that turn's AI reply.
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
        // While an option slot is up, the meaning of send changes: it means "this is my answer" rather than starting...
        let choosing = self.choice.is_some();
        // Attachments with no text may be sent too (dragging an image in and pressing send is the most common usage).
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

                // The option slot: a multiple-choice question the model asked via `ask_user`. It goes above the pending-upload...
                self.choice_bar(ui);

                // The pending-upload bar: the row of small chips **above** the input card (the
                // position the user explicitly asked for on 2026-10-05).
                self.attach_bar(ui);

                // The floating input box: no separator line and no fill around it, floating above the transcript on a large...
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
                        // The plan lives inside the input card, above a hairline divider: as a card of its own it left a seam against...
                        self.plan_bar(ui);
                        // Note: ui.horizontal_centered() can't be used here -- it requests "the full available size" in one go...
                        ui.horizontal(|ui| {
                            let hint = if self.demo {
                                aox::i18n::t("Demo mode: layout preview only (start with --demo; run normally to chat)")
                            } else if blocked {
                                aox::i18n::t("Model not configured; chat unavailable")
                            } else if self.waiting_auth {
                                aox::i18n::t("Waiting for authorization…")
                            } else if choosing {
                                // Point at the inline input box on the row above: the place to answer is beside the question, and people...
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
                            // ── Height cap: grows to seven rows, then scrolls ──.
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

                        // Second row: the workplace button at the bottom-left, the usage readouts at the bottom-right.
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            // The model chip comes first, before the workplace chip: it answers "who is answering", which is the more...
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
                            // The recovery chip: one step to the right of the workspace button, because both answer "what can...
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
                                // While the host is folding old turns into a summary, the gauge has no honest reading (the numbers only come back...
                                let spin = self.compacting.map(|t| t.elapsed().as_secs_f32());
                                context_ring(ui, self.context_used(), self.context_window, spin);
                                if spin.is_some() {
                                    ui.add_space(6.0);
                                    ui.label(
                                        RichText::new(aox::i18n::t("Compacting context…"))
                                            .size(11.0)
                                            .color(P::text_dim()),
                                    );
                                    // Nothing else in this row animates, so the repaint has to be asked for by hand; without it the arc would jump...
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
        // First frame: nothing measured yet, so exactly one row
        assert_eq!(input_box_height(None, row_h), row_h);
        // A short document: the box follows the text
        assert_eq!(input_box_height(Some(60.0), row_h), 60.0);
        // Exactly at the cap: still the cap
        assert_eq!(input_box_height(Some(row_h * 7.0), row_h), row_h * 7.0);
        // Past the cap: the box stops growing and the rest scrolls
        assert_eq!(input_box_height(Some(row_h * 40.0), row_h), row_h * 7.0);
        // A stale tiny reading (text just cleared) cannot collapse the box.
        assert_eq!(input_box_height(Some(1.0), row_h), row_h);
    }
}
