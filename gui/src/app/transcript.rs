//! The conversation itself: past items, the live turn, and the rows inside them.
//!
//! Everything that ends up in the scrolling column is painted here -- bubbles, Markdown replies, notes,
//! the process card and its per-call rows with their diffs, images, and the reasoning panel. The
//! division from `composer` is "above the input card" versus "the card": the two share the same
//! `composer_top` boundary, which is why the transcript's viewport height is computed from it.

use super::*;

/// Breathing room between the transcript's last line and the composer's top edge. The viewport ends
/// at the card; this keeps its shadow from touching the text.
pub(super) const TRANSCRIPT_BOTTOM_GAP: f32 = 18.0;

/// Flatten the history messages read from disk into the transcript's turn-by-turn records.
///
/// The only difference from live rendering is how the "process" is accumulated: live accumulates
/// as events arrive, whereas here it is reconstructed from the message sequence -- an assistant's
/// tool_calls open a process, and the tool results right after it are filled back into the
/// matching step by name.
pub(super) fn items_from_history(loaded: &[aox::history::Loaded]) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    // The process currently being accumulated (settled and pushed into `out` when a new user
    // message or a final answer appears)
    let mut pending: Option<Process> = None;

    let flush = |out: &mut Vec<Item>, pending: &mut Option<Process>| {
        if let Some(p) = pending.take() {
            out.push(Item::Process(p));
        }
    };

    for it in loaded {
        let m = &it.msg;
        match m.role.as_str() {
            "user" => {
                // `flat_text` rather than `content.clone()`: a user message may carry attachments,
                // in which case content is a parts array -- cloning it directly shows not a single
                // character. The attachment manifest block the host adds itself is filtered out by
                // `flat_text` (see `attach::ATTACH_MARK`).
                let text = m.flat_text();
                // Both the chip's name and the image paths to **display** are recovered from the
                // manifest block: the `image_url` block has no file-name field (see
                // `attach::files_from_content`).
                let files = m
                    .content
                    .as_ref()
                    .map(aox::attach::files_from_content)
                    .unwrap_or_default();
                let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
                let mut paths: Vec<PathBuf> = files.into_iter().map(|(_, p)| p).collect();
                let from_host = text.trim_start().starts_with(aox::history::HOST_PREFIX);

                // A host message carrying files is the protocol envelope for what a tool just
                // produced. It is **not** conversation content: the "(host hint) …" sentence was
                // never meant for the person, and the images belong under the call that made them.
                // `flat_text` also lists the attachments separately, so `text` would only repeat
                // what the tool row already says.
                if from_host && !paths.is_empty() {
                    if let Some(p) = pending.as_mut() {
                        attach_history_images(p, &paths);
                    } else {
                        // No call to attach to (a truncated/old file): keep the pictures visible,
                        // but still do not print the host sentence.
                        out.push(Item::Images(paths));
                    }
                    continue;
                }

                flush(&mut out, &mut pending);
                if from_host {
                    out.push(Item::Note(NoteKind::Warn, text.trim().to_string()));
                } else {
                    out.push(Item::User(text, names));
                    // Only a **user's** own attachments get their own block. Tool attachments were
                    // handled above, on the call they belong to.
                    if !paths.is_empty() {
                        out.push(Item::Images(std::mem::take(&mut paths)));
                    }
                }
            }
            "assistant" => {
                let calls = m.tool_calls.clone().unwrap_or_default();
                let text = m.flat_text();
                if calls.is_empty() {
                    flush(&mut out, &mut pending);
                    if !text.trim().is_empty() {
                        out.push(Item::Assistant(text));
                    }
                } else {
                    let p = pending.get_or_insert_with(|| Process {
                        steps: Vec::new(),
                        interim: Vec::new(),
                        reasoning: String::new(),
                        // History has no elapsed time (that is a runtime-only fact), so it isn't shown
                        elapsed: None,
                        expanded: false,
                        aborted: false,
                    });
                    if let Some(r) = m.reasoning_content.as_deref() {
                        p.reasoning.push_str(r);
                    }
                    if !text.trim().is_empty() {
                        p.interim.push(text);
                    }
                    for c in &calls {
                        p.steps.push(StepView {
                            name: c.function.name.clone(),
                            plugin: None,
                            // The file name still comes from the arguments, which the history file
                            // does keep — only the diff itself was never written down.
                            subject: subject_from_json(
                                &c.function.name,
                                &c.function.arguments,
                            ),
                            purpose: purpose_from_json(
                                &c.function.name,
                                &c.function.arguments,
                            ),
                            args: c.function.arguments.trim().to_string(),
                            result: None,
                            // Not persisted, so a resumed turn shows no figure on its edit rows.
                            diff: None,
                            diff_detail: None,
                            expanded: false,
                            images: Vec::new(),
                        });
                    }
                }
            }
            "tool" => {
                let name = m.name.clone().unwrap_or_default();
                let body = m.flat_text();
                if let Some(p) = pending.as_mut() {
                    // The same step may have several calls of the same name; fill back into "the last
                    // same-named step that has no result yet"
                    if let Some(st) = p
                        .steps
                        .iter_mut()
                        .rev()
                        .find(|s| s.name == name && s.result.is_none())
                    {
                        st.result = Some((it.is_error == Some(true), body));
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut out, &mut pending);
    out
}

pub(super) fn user_bubble(ui: &mut Ui, text: &str, files: &[String], max_w: f32) {
    // The padding takes up width on top, so the body text's available width has to be subtracted
    // first; otherwise the bubble sticks out of the column and gets clipped
    let inner_w = (max_w - 44.0).max(140.0);
    let body = text.trim();

    // Lay the body text out once at `inner_w` first and **measure the width actually used**: short
    // sentences narrow, long ones run full.
    //
    // ⚠️ This step can't be skipped. `Frame::show` computes its own rect from the content's
    // `min_rect`, and that is stretched to "available width" from the start -- writing only
    // `ui.set_max_width(..)` **clamps that width down** rather than letting it shrink with the
    // content. The result: however long the message, the bubble is the same width (which is exactly
    // what the user reported on 2026-10-06).
    let galley = (!body.is_empty()).then(|| {
        ui.painter().layout(
            body.to_owned(),
            theme::body_font(theme::body_size()),
            P::text(),
            inner_w,
        )
    });
    let text_w = galley.as_ref().map_or(0.0, |g| g.size().x);
    // With attachments the bubble has to fit at least one chip (the widest one is used as the lower
    // bound; if it really doesn't fit, let it wrap)
    let chips_w = files.iter().map(|n| chip_width(ui, n)).fold(0.0_f32, f32::max);
    let want = text_w.max(chips_w).min(inner_w);

    ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
        Frame::NONE
            .fill(P::user_bg())
            .corner_radius(CornerRadius::same(theme::R3))
            .stroke(Stroke::new(1.0, P::edge()))
            .inner_margin(Margin::symmetric(20, 15))
            .show(ui, |ui| {
                // The width is **pinned** (min and max both set to the same value), so the content's
                // `min_rect` is `want` and the bubble closes around the content -- a narrow strip for a
                // short sentence, the full measure for a long one.
                ui.set_min_width(want);
                ui.set_max_width(want);
                ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                    // The attachment chips go **above** the body text: consistent with their position
                    // in the pending-upload bar (the same look before and after sending), and with an
                    // empty body (only files sent) the bubble still has something to show.
                    if !files.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(7.0, 7.0);
                            for name in files {
                                sent_chip(ui, name);
                            }
                        });
                        if !text.trim().is_empty() {
                            ui.add_space(8.0);
                        }
                    }
                    if text.trim().is_empty() {
                        return;
                    }
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = inner_w;
                    job.append(
                        text,
                        0.0,
                        egui::TextFormat {
                            font_id: theme::body_font(theme::body_size()),
                            color: P::text(),
                            ..Default::default()
                        },
                    );
                    ui.add(Label::new(job).selectable(true).wrap_mode(egui::TextWrapMode::Wrap));
                });
            });
    });
    ui.add_space(18.0);
}

pub(super) fn assistant_block(ui: &mut Ui, text: &str, width: f32) {
    markdown::render(ui, text, P::text());
    let _ = width;
}

/// Extensions that look like images. **Only used to decide "is it worth trying to decode"**, not
/// to determine the format -- the decoder itself (`aox::image::decode`) is what actually decides.
/// So a file whose extension isn't in the table but whose content is really a PNG won't be shown as
/// an image, which is the price of this shortcut; and conversely "the extension looks like an image
/// but the content isn't" gets rejected by the decoder, and the UI says "display failed" honestly
/// rather than pretending it worked.
pub(super) fn looks_like_image(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
            )
        })
}

/// An image carried by a message. Scaled to the measure's width, aspect ratio preserved.
///
/// Each of the three states is drawn its own way (see [`Tex`]): the image / "decoding..." / a
/// one-line failure reason.
/// **Never upscaled** (`scale` capped at 1.0): blowing a 200x80 diagram up to the full measure only
/// smears it into mosaic, and the user seeing "this is how big it is" is more useful than seeing a
/// blurred large version.
pub(super) fn images_block(ui: &mut Ui, paths: &[PathBuf], inner: f32, tex: &HashMap<PathBuf, Tex>) {
    for p in paths {
        if !looks_like_image(p) {
            // Not an image (.txt / .zip and the like): there was never meant to be a picture, so
            // skip quietly. A user's bubble lists its file names; a tool's row names the tool and
            // its result already says what was attached, so neither needs a second line here.
            continue;
        }
        ui.add_space(6.0);
        match tex.get(p) {
            Some(Tex::Ready(h)) => {
                let size = h.size_vec2();
                let scale = (inner.min(760.0) / size.x).min(460.0 / size.y).min(1.0);
                let show = size * scale.max(0.05);
                ui.add(egui::Image::new(egui::load::SizedTexture::new(h.id(), show)).corner_radius(12.0));
            }
            Some(Tex::Failed(e)) => {
                ui.label(
                    RichText::new(aox::tf!("Cannot display this image: {}", e))
                        .color(P::text_faint())
                        .size(12.5),
                );
            }
            // Queued / decoding. To the user these are the same thing; no need to say them
            // separately.
            _ => {
                ui.label(RichText::new(aox::i18n::t("Decoding image…")).color(P::text_faint()).size(12.5));
            }
        }
        ui.add_space(2.0);
    }
    ui.add_space(4.0);
}

pub(super) fn note_row(ui: &mut Ui, kind: NoteKind, text: &str) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, 20.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, kind.color());
        ui.add_space(4.0);
        ui.label(RichText::new(text).color(kind.color()).size(14.0));
    });
    ui.add_space(9.0);
}

pub(super) fn error_card(ui: &mut Ui, text: &str, width: f32) {
    // In a monochrome scheme, errors use a "reversed block" rather than red text -- the highest
    // contrast, and the least dependent on hue
    let inner_w = (width - 40.0).max(140.0);
    Frame::NONE
        .fill(P::surface())
        .corner_radius(CornerRadius::same(theme::card_radius()))
        .stroke(Stroke::new(1.0, P::edge_raised()))
        .shadow(theme::float_shadow())
        .inner_margin(Margin::symmetric(20, 18))
        .show(ui, |ui| {
            ui.set_width(inner_w);
            ui.set_max_width(inner_w);
            Frame::NONE
                .fill(P::reverse_bg())
                .corner_radius(CornerRadius::same(theme::R0))
                .inner_margin(Margin::symmetric(11, 4))
                .show(ui, |ui| {
                    ui.label(RichText::new(aox::i18n::t("Failed")).color(P::on_reverse()).size(14.0));
                });
            ui.add_space(10.0);
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = inner_w;
            job.wrap.break_anywhere = true;
            job.append(
                &clamp_str(text, 2000),
                0.0,
                egui::TextFormat {
                    font_id: theme::body_font(15.0),
                    color: P::text_dim(),
                    ..Default::default()
                },
            );
            ui.add(Label::new(job).selectable(true));
        });
    ui.add_space(18.0);
}

pub(super) fn process_block(ui: &mut Ui, p: &mut Process, width: f32, tex: &HashMap<PathBuf, Tex>) {
    let n = p.steps.len();
    // History has no elapsed-time information (that only exists at runtime), so only the step
    // count is reported here.
    let tail = match p.elapsed {
        Some(s) => format!(" · {s:.1}s"),
        None => String::new(),
    };
    let label = if p.aborted {
        aox::tf!("Task interrupted · {} steps{}", n, tail)
    } else if n == 0 {
        aox::tf!("Processing{}", tail)
    } else {
        aox::tf!("{} tool calls{}", n, tail)
    };

    if n == 0 && p.interim.is_empty() && p.reasoning.is_empty() {
        return;
    }

    let row_w = width.min(620.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(row_w, 30.0), Sense::click());
    if pointer_if(ui, &resp) {
        // Hover lifts the row: a 1 px shade under it before the fill, which is the same "touched,
        // not selected" move list rows make. A bare fill on a page this close in value read as a
        // slightly paler stripe rather than as a row being pointed at (2026-10-29).
        let p = ui.painter();
        p.rect_filled(
            rect.translate(egui::vec2(0.0, 1.0)),
            CornerRadius::same(theme::R1),
            P::shade(0.06),
        );
        p.rect_filled(rect, CornerRadius::same(theme::R1), P::surface());
    }
    paint_tri(
        ui.painter(),
        Pos2::new(rect.left() + 11.0, rect.center().y),
        4.6,
        p.expanded,
        if pointer_if(ui, &resp) { P::text_dim() } else { P::text_faint() },
    );
    ui.painter().text(
        Pos2::new(rect.left() + 26.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::body_font(14.0),
        P::text_faint(),
    );
    if resp.clicked() {
        p.expanded = !p.expanded;
    }
    ui.add_space(7.0);

    // Collapsed, the individual rows are not drawn — but a picture the tool produced must not
    // disappear behind "12 tool calls". Show the batch's images under the summary row; expanded,
    // each one is drawn under its own call (see `step_row_inner`).
    if !p.expanded {
        let images: Vec<PathBuf> = p
            .steps
            .iter()
            .flat_map(|s| s.images.iter().cloned())
            .collect();
        if !images.is_empty() {
            images_block(ui, &images, width, tex);
        }
    }

    if p.expanded {
        Frame::NONE
            .fill(P::surface())
            .corner_radius(CornerRadius::same(theme::card_radius()))
            .stroke(Stroke::new(1.0, P::border()))
            .inner_margin(Margin::symmetric(20, 18))
            .show(ui, |ui| {
                // Note: the available width **inside the frame** has to be used here.
                // Reusing the outer width would make the content wider than the frame's inner area,
                // and the whole card would be pushed past the column width and clipped.
                let w = ui.available_width();
                ui.set_width(w);
                ui.set_max_width(w);
                for (i, st) in p.steps.iter_mut().enumerate() {
                    step_row(ui, st, i, tex);
                }
                if !p.interim.is_empty() {
                    ui.add_space(8.0);
                    for t in &p.interim {
                        ui.label(
                            RichText::new(clamp_str(t, 400))
                                .color(P::text_faint())
                                .size(14.0),
                        );
                    }
                }
                if !p.reasoning.is_empty() {
                    ui.add_space(12.0);
                    ui.label(RichText::new(aox::i18n::t("Reasoning")).color(P::text_faint()).size(13.0));
                    ui.add_space(5.0);
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = w;
                    job.wrap.break_anywhere = true;
                    job.append(
                        &clamp_str(&p.reasoning, 3000),
                        0.0,
                        egui::TextFormat {
                            font_id: theme::body_font(14.0),
                            color: P::text_faint(),
                            ..Default::default()
                        },
                    );
                    ui.add(Label::new(job).selectable(true).wrap_mode(egui::TextWrapMode::Wrap));
                }
            });
        ui.add_space(16.0);
    }
}

/// A stretch of "what the model is saying right now" in the live area.
///
/// Deliberately rendered as **plain body text**, with no Markdown and no bubble border: it is a
/// temporary form of the same text before it hits disk, and once saved it becomes a bubble with
/// Markdown. Introducing a different style midway would make the two look like different sentences
/// and "jump" on the switch. `break_anywhere` is on to stop long paths in the answer from bursting
/// the measure -- all that matters here is not overflowing; whether it looks good is left to the
/// final bubble.
pub(super) fn live_plain_text(ui: &mut Ui, text: &str, color: Color32) {
    let w = ui.available_width();
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = w;
    job.wrap.break_anywhere = true;
    job.append(
        text,
        0.0,
        egui::TextFormat { font_id: theme::body_font(15.0), color, ..Default::default() },
    );
    ui.add(Label::new(job));
}

/// One call row, plus the diff panel it can unfold into.
///
/// `idx` is the row's slot in its container and is used **only** to give the row an id of its own.
/// Two rows in one card can both be unfolded, and each panel is *its own* scroll area and *its own*
/// draggable height — with a shared id they fight over one offset and one size (user report,
/// 2026-10-11: "unfold two diff panels and scrolling one scrolls the other too").
pub(super) fn step_row(ui: &mut Ui, st: &mut StepView, idx: usize, tex: &HashMap<PathBuf, Tex>) {
    ui.push_id(idx, |ui| step_row_inner(ui, st, tex));
}

pub(super) fn step_row_inner(ui: &mut Ui, st: &mut StepView, tex: &HashMap<PathBuf, Tex>) {
    // A row with a diff behind it is **foldable** (user request, 2026-10-11: "the file being edited,
    // and clicking it opens the change"). Every other row stays plain and inert.
    let foldable = st.diff_detail.is_some();
    let unfolded = st.expanded;
    // The file name, if it resolves to something on disk. Collected inside the row and used *after*
    // the row's own click is registered -- see below.
    let mut name_hit: Option<(egui::Rect, std::path::PathBuf)> = None;
    let row = ui
        .horizontal_top(|ui| {
            // Status markers (the form settled on with the user, 2026-10-05): in progress = a three-dot
            // marquee (**deliberately a different shape from the 8-dot orbiting spinner used for
            // reasoning**), success = ✓, failure = ✗.
            // They are no longer numbered either -- numbering reads "a few calls in one stage" as a
            // checklist, whereas what is wanted here is "one after another".
            match &st.result {
                None => call_marquee(ui, P::text_dim()),
                Some((is_err, _)) if *is_err => call_glyph(ui, false, P::ink()),
                Some(_) => call_glyph(ui, true, P::ink()),
            }
            ui.add_space(4.0);
            ui.label(RichText::new(&st.name).color(P::text()).size(14.5));
            // Why this call exists, between the name and the file it works on: "why, then where" is
            // the order people read them, and without it a row reading `bash` said nothing at all
            // about what was about to run. Dimmer than the name so rows stay scannable by tool name.
            if let Some(why) = &st.purpose {
                ui.add_space(6.0);
                ui.label(RichText::new(why).color(P::text_dim()).size(13.5));
            }
            // The file this call works on, straight after its name (user request, 2026-10-11): a row
            // reading only "write_file" leaves you counting back to find out which file it was.
            // Monospaced so two paths line up, and shortened from the middle -- the tail is the half
            // that identifies a file.
            if let Some(sub) = &st.subject {
                ui.add_space(6.0);
                // Clickable when it resolves inside the workspace: "which file was that" is one
                // click from "show me". The rect is collected here and re-registered after the row's
                // own click below -- see the note there.
                let path = crate::open::resolve_relative(sub);
                let resp = ui.add(
                    Label::new(
                        RichText::new(clip_path(sub))
                            .font(theme::mono_font(13.0))
                            .color(P::text_dim()),
                    )
                    .selectable(false)
                    .sense(Sense::click()),
                );
                if path.is_some() {
                    pointer_if(ui, &resp);
                }
                if let Some(p) = path {
                    name_hit = Some((resp.rect, p));
                }
            }
            if let Some(pl) = &st.plugin {
                ui.add_space(2.0);
                Frame::NONE
                    .fill(P::surface_hi())
                    .corner_radius(CornerRadius::same(theme::pill(22.0)))
                    .inner_margin(Margin::symmetric(9, 2))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(if pl == "meta" { "meta" } else { pl.as_str() })
                                .color(P::text_dim())
                                .size(12.0),
                        );
                    });
            }
            // Edit statistic, right where the eye already is — next to the call it belongs to. Green
            // for added lines, red for removed, and **nothing at all** when the call changed no lines:
            // a "0 / 0" would just be noise on every read-only call.
            if let Some((added, removed)) = st.diff {
                if added > 0 {
                    ui.add_space(6.0);
                    ui.label(RichText::new(format!("+{added}")).color(P::add_green()).size(13.0));
                }
                if removed > 0 {
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("-{removed}")).color(P::del_red()).size(13.0));
                }
            }
            // The caret sits at the far right edge of the row. It is also what makes the **whole
            // row** the click target: "there is something to open here".
            if foldable {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(14.0, 18.0), Sense::hover());
                    paint_tri(ui.painter(), rect.center(), 4.0, unfolded, P::text_faint());
                });
            }
            // Arguments are **not shown here** (user request, 2026-10-05): a JSON blob would stretch
            // this row into several lines, and expanded you can't see the bottom of it. The row keeps
            // only "tool name + owning plugin", and the next row gives a one-line summary of the result.
            // To really see the arguments, they are stored verbatim in `history/*.jsonl`.
        })
        .response;
    if foldable {
        // Re-register the row's own rect with a click sense, **after** the row has been built: egui
        // hands a click to the topmost widget that wants it, and the labels inside the row are added
        // before this, so the row wins.
        let resp = row.interact(Sense::click());
        // ...and for the same reason the file name has to be registered *after that* to win in turn:
        // on a foldable row a click on the path must reveal the file, not fold the diff shut.
        if let Some((rect, path)) = name_hit {
            let name = ui
                .interact(rect, ui.id().with("aox-step-file"), Sense::click())
                .on_hover_text(path.display().to_string());
            if name.clicked() {
                crate::open::reveal(&path);
            }
        }
        if pointer_if(ui, &resp) && resp.clicked() {
            st.expanded = !st.expanded;
        }
    }
    // Directly below the call, indented under its text column: the answer to "what did this step
    // produce" sits next to the step, not in a separate block at the top of the conversation.
    if !st.images.is_empty() {
        ui.horizontal_top(|ui| {
            ui.add_space(20.0);
            ui.vertical(|ui| {
                let w = ui.available_width();
                images_block(ui, &st.images, w, tex);
            });
        });
    }
    if st.expanded {
        // Borrowed, never cloned: the transcript is laid out again on every repaint, and this string
        // is the length of the diff.
        if let Some(diff) = st.diff_detail.as_deref() {
            step_diff(ui, diff);
        }
    }
    ui.add_space(5.0);
}

/// The unfolded edit: every line of the diff, tinted by what the line is.
///
/// A **short** panel that scrolls (user request, 2026-10-11). The question it answers is "which lines
/// moved", and a panel that grew with the file would push the conversation off screen. Its cap is
/// [`DIFF_PANEL_H`], and a diff shorter than that draws a shorter panel — a small edit leaves no
/// empty background behind.
pub(super) fn step_diff(ui: &mut Ui, diff: &str) {
    ui.add_space(6.0);
    Frame::NONE
        .fill(P::code_bg())
        .corner_radius(CornerRadius::same(theme::R1))
        .stroke(Stroke::new(1.0, P::edge()))
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            // Inside the frame, so the panel can never be wider than the card holding it.
            let w = ui.available_width();
            ui.set_width(w);
            ui.set_max_width(w);
            // Claim the panel's height back from egui before building it.
            //
            // egui gives a nested `ScrollArea` only the height left in its parent's `max_rect`, and
            // inside the transcript **every** parent's rect is one screen tall, however far down the
            // page the row sits. A panel opened near the bottom of a long conversation was therefore
            // handed a few dozen pixels and stayed that size: `max_height(DIFF_PANEL_H)` is a cap, not
            // a request, and nothing could reach it (user report, 2026-10-11: "still too short", with
            // a three-line panel under a +353 diff). `set_max_height` grows the rect from the cursor
            // downwards, so asking for the full panel height here is exactly the fix.
            ui.set_max_height(ui.available_height().max(DIFF_PANEL_H));
            // The first character is the whole contract (see `diff_text`): `+` added, `-` removed, a
            // space for context, and anything else is a marker — an elided stretch, or where a diff
            // too large to draw was cut off — drawn faintest of all.
            ScrollArea::vertical()
                .id_salt("aox-step-diff")
                .max_height(DIFF_PANEL_H)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for line in diff.lines() {
                        let color = match line.chars().next() {
                            Some('+') => P::add_green(),
                            Some('-') => P::del_red(),
                            Some(' ') => P::text_dim(),
                            _ => P::text_faint(),
                        };
                        ui.label(RichText::new(line).font(theme::mono_font(12.5)).color(color));
                    }
                });
        });
}

/// A stretch of call stream in the live area (the calls preceding one interim reply).
///
/// The live area gets **one card per stretch** (see the segmentation loop in `live_block`), whereas
/// the process card puts everything in a single card -- the row rendering is shared, only the
/// grouping differs.
pub(super) fn calls_card(ui: &mut Ui, steps: &mut [StepView], tex: &HashMap<PathBuf, Tex>) {
    theme::card(theme::card_radius())
        .inner_margin(Margin::symmetric(20, 16))
        .show(ui, |ui| {
            // Use the available width inside the frame, so the content can't push the card out of the
            // column width
            let w = ui.available_width();
            ui.set_width(w);
            ui.set_max_width(w);
            for (i, st) in steps.iter_mut().enumerate() {
                // Each row ids itself off its slot (see `step_row`), so panels never share a scroll
                // offset or a height.
                step_row(ui, st, i, tex);
            }
        });
}

/// The "in progress" marker in the call stream: three dots with the lit one moving along.
///
/// **Deliberately a different shape** from [`spinner_dot`]'s 8-dot orbit (the user asked for
/// "something distinct from the reasoning spinner"): one glance tells you whether it is calling a
/// tool or thinking.
pub(super) fn call_marquee(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 20.0), Sense::hover());
    // The phase comes from `ui.input(|i| i.time)` (egui's self-incrementing seconds count):
    // callable anywhere directly, with no need to thread `App`'s `spin` all the way down.
    let t = ui.input(|i| i.time) as f32;
    let active = ((t * 3.0) as usize) % 3;
    let painter = ui.painter();
    for i in 0..3 {
        let hot = i == active;
        let p = Pos2::new(rect.left() + 3.0 + i as f32 * 5.0, rect.center().y);
        painter.circle_filled(
            p,
            if hot { 2.4 } else { 1.8 },
            color.gamma_multiply(if hot { 1.0 } else { 0.22 }),
        );
    }
}

/// ✓ / ✗. Hand-drawn from two line segments -- no betting on whether the font has these two
/// glyphs (the same reason as `paint_folder`).
pub(super) fn call_glyph(ui: &mut Ui, ok: bool, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 20.0), Sense::hover());
    let c = rect.center();
    let s = 4.4;
    let stroke = Stroke::new(1.8, color);
    let painter = ui.painter();
    if ok {
        painter.line_segment(
            [Pos2::new(c.x - s, c.y + 0.6), Pos2::new(c.x - 1.4, c.y + s)],
            stroke,
        );
        painter.line_segment([Pos2::new(c.x - 1.4, c.y + s), Pos2::new(c.x + s, c.y - s)], stroke);
    } else {
        painter.line_segment([Pos2::new(c.x - s, c.y - s), Pos2::new(c.x + s, c.y + s)], stroke);
        painter.line_segment([Pos2::new(c.x + s, c.y - s), Pos2::new(c.x - s, c.y + s)], stroke);
    }
}

pub(super) fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.trim_end().lines().collect();
    let start = lines.len().saturating_sub(n);
    let tail = lines[start..].join("\n");
    clamp_str(&tail, 900)
}

/// Whether the live reasoning panel is worth drawing at all.
///
/// Whitespace does **not** count as reasoning: several providers open the field with a bare newline
/// (or a run of them) before the first real token, and a card holding an empty line is exactly the
/// blank rectangle this hides. The panel is 106 px of card — it earns that only once there is a
/// sentence to read.
pub(super) fn has_reasoning(text: &str) -> bool {
    !text.trim().is_empty()
}

/// Is the reasoning band (the four faint lines at the bottom of the live area) on screen?
///
/// Only while the model is **thinking and not yet writing** (2026-10-30). `typing` is "there is body
/// text on screen right now", and the moment it is true the band goes: what it still holds is the
/// tail of the reasoning that *led to* this answer, i.e. the last thoughts before the model decided
/// to write — sitting under a reply that is already being printed. Reported with a screenshot: one
/// leftover thinking line plus a "Thinking 16.4s · ~2,637 tok" head below a finished-looking answer.
///
/// It comes back on its own during a tool run: narration clears the body text (`InterimText`), so the
/// band reappears next to the calls it explains — where it is not stale and nothing is streaming past
/// it.
pub(super) fn thinking_band_visible(reasoning: bool, typing: bool) -> bool {
    reasoning && !typing
}

/// Does the live area draw the spinner-and-"Thinking" head above its readouts?
///
/// Two ways to earn it, and **both are about telling the truth**:
///
/// - the reasoning band is on screen — the head is then its caption;
/// - or the status says something that is not the default "Thinking" ("Running shell_exec",
///   "Retrying in 3 s", "Waiting for your approval"…). Those are facts, and hiding them would leave
///   a stalled turn with nothing to explain it.
pub(super) fn thinking_head_visible(status: &str, thinking: bool) -> bool {
    thinking || (!status.is_empty() && status != aox::i18n::t("Thinking"))
}

pub(super) fn compact_json(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_else(|_| v.to_string());
    clamp_str(&s, 300)
}

// ────────────── "back to the start of this reply" ──────────────

/// Breathing room above a reply after a jump: its first line lands this far below the viewport's top
/// edge instead of butting against it.
const JUMP_MARGIN: f32 = 12.0;

/// The chip's box on the usage line. Also the slot the line reserves for it — see [`usage_row`] for
/// why the slot is reserved even when the chip is not drawn.
pub(super) const JUMP_CHIP: f32 = 18.0;

/// "To the top": a bar with an arrow climbing into it, painted from line segments.
///
/// Drawn rather than set as a character, the same reason as [`call_glyph`]: `↑` / `⇱` are not in
/// every face this app ships with, and a missing glyph in an 18-pixel chip is a box with a hex
/// number in it.
fn jump_arrow(p: &egui::Painter, c: Pos2, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    // The bar the arrow arrives at.
    p.line_segment([Pos2::new(c.x - 4.6, c.y - 5.6), Pos2::new(c.x + 4.6, c.y - 5.6)], stroke);
    // Shaft, then the two sides of the head. The head is drawn slightly wide of the shaft so the
    // join does not look thicker than the rest at 1.5 px.
    p.line_segment([Pos2::new(c.x, c.y + 5.4), Pos2::new(c.x, c.y - 3.4)], stroke);
    p.line_segment([Pos2::new(c.x - 3.6, c.y - 0.4), Pos2::new(c.x, c.y - 4.2)], stroke);
    p.line_segment([Pos2::new(c.x + 3.6, c.y - 0.4), Pos2::new(c.x, c.y - 4.2)], stroke);
}

/// The chip itself: invisible until reached for, exactly like the copy button on a code block.
pub(super) fn jump_chip(ui: &mut Ui) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(JUMP_CHIP), Sense::click());
    let hot = resp.hovered();
    if hot {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R1), P::surface_hi());
    }
    jump_arrow(ui.painter(), rect.center(), if hot { P::text() } else { P::text_faint() });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(aox::i18n::t("Back to the start of this reply"))
        .clicked()
}

// ────────────── the conversation rail ──────────────

/// How long the rail takes to slide in — or out — in seconds.
///
/// Short on purpose. It is not a transition anyone should watch; it exists so a third question
/// does not make a column of marks appear out of nowhere while the reader is mid-sentence.
const RAIL_SECS: f32 = 0.22;

/// How many questions a conversation needs before the rail is drawn at all.
///
/// Two ticks are not a map, they are a pair of dots, and the reader can already see both ends of a
/// two-turn conversation. Three is where "where am I in this thing" starts being a real question.
const RAIL_MIN: usize = 3;

/// Space between the track's ends and the edges it runs between (the title bar's bottom edge, and
/// the composer's top edge).
const RAIL_INSET: f32 = 16.0;

/// How far inside the scroll area's right edge the track must stay, clear of the scroll bar
/// (8 wide + 3 inner margin + 4 outer).
///
/// Only a ceiling: the track's x is the **middle of the gutter** between the text and the right
/// edge, which moves with the window (2026-10-31: a fixed offset from the column put the rail in the
/// same spot at every width, and at 1080 it hugged the text while ~130 points of margin sat empty on
/// its right).
const RAIL_FROM_BAR: f32 = 16.0;

/// A tick's size at rest, and while it is under the pointer.
const RAIL_H: f32 = 3.0;
const RAIL_W: f32 = 10.0;
const RAIL_W_CUR: f32 = 16.0;

/// The box that marks the turn being read: hollow, `RAIL_W_CUR` wide and this tall.
///
/// It used to be the same 3-point bar, just wider and darker — which at a glance was no different
/// from a tick under the pointer (2026-10-31: "只是那个节点条变宽一点，不太明显"). A hollow rectangle
/// is a different *shape*, not a different weight, so the eye picks it out of the column without
/// making it heavier than the text beside it.
const RAIL_BOX_H: f32 = 9.0;

/// Vertical pitch between two questions, i.e. the height of one row of the index.
///
/// The rail went through two earlier shapes before this: proportional to the whole document (which
/// scattered three ticks over 500 points of empty margin — "太丑了"), then proportional *within* a
/// centred band (better, but the gaps still differed: "间距还是不一样，应该是等间距的"). Even
/// spacing is what a list of turns actually wants: it is an index you click, not a minimap, and the
/// filled tick — not the position — is what says which turn you are in.
const RAIL_STEP: f32 = 34.0;

/// Where each tick sits, in screen y: `RAIL_STEP` apart, the list centred in the track.
///
/// Evenly spaced, deliberately. The ticks used to be proportional to the conversation (first to the
/// whole document, then to the questions' own span), which made two neighbouring questions look
/// misaligned when one reply happened to be long — "间距还是不一样". Equal spacing is what makes the
/// column read as a list, and a list is the right metaphor for a set of jumps: the position says
/// nothing, the filled tick says everything.
///
/// The pitch is capped so a long conversation still fits the track; past that the ticks pack
/// tighter instead of running out over the composer.
fn rail_tick_ys(n: usize, top: f32, len: f32) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![top + len * 0.5];
    }
    let pitch = RAIL_STEP.min(len / (n as f32 - 1.0));
    let height = pitch * (n as f32 - 1.0);
    let first = top + (len - height) * 0.5;
    (0..n).map(|i| first + pitch * i as f32).collect()
}

/// Which question the viewport is sitting in: the last one whose head is at or above the top edge.
///
/// The `unwrap_or(0)` is not a fallback for an empty list (there is none — the rail needs three) but
/// for the very top of the document, where the first question is a few points *below* the top edge
/// (the transcript starts with 76 points of padding) and is unmistakably the one being read.
fn rail_current(qy: &[f32], offset: f32) -> usize {
    qy.iter().rposition(|y| *y <= offset + 8.0).unwrap_or(0)
}

/// The question, flattened to one run of text for the hover card.
///
/// Newlines and runs of spaces are collapsed, because a bubble may well start with an indented
/// code block and the card is two lines tall — a leading `    ` would eat the whole card. The cap
/// is generous (the galley elides by pixels); it exists only so a pasted log cannot be laid out
/// hundreds of rows deep just to be cut off.
fn rail_question_text(t: &str) -> String {
    const MAX: usize = 160;
    let mut out = String::new();
    let mut space = false;
    let mut n = 0usize;
    for ch in t.chars() {
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(ch);
        n += 1;
        if n >= MAX {
            break;
        }
    }
    out
}

/// How much work the turn behind a question took: tool calls made, and how long they ran.
///
/// Walked from that question to the next one, so it can only ever report the turn it was asked
/// about. `None` for the seconds when they were never recorded (history files carry no timing),
/// which is also why the card omits the duration rather than printing `0.0s`.
fn rail_turn(items: &[Item], start: usize) -> (usize, Option<f32>) {
    let mut calls = 0;
    let mut secs: Option<f32> = None;
    for it in items.get(start + 1..).unwrap_or(&[]) {
        match it {
            Item::User(..) => break,
            Item::Process(p) => {
                calls += p.steps.len();
                if let Some(e) = p.elapsed {
                    *secs.get_or_insert(0.0) += e;
                }
            }
            _ => {}
        }
    }
    (calls, secs)
}

/// Lay out `rows` lines of text, elided by **pixels** at the last row — [`elide`] for a block that
/// is allowed to wrap once.
///
/// `break_anywhere` is **off** here, unlike [`elide`]: that flag is what lets a Chinese sentence be
/// cut, but it also breaks at the glyph that fits rather than at the space before it, so a Latin
/// question came out as "…files by type i / n the current directory". The price is that one
/// unbroken run (a path, a URL) overflows its row — which is why the caller clips the card: a path
/// cut off at the card's edge reads as "there is more", while a word split in half reads as a bug.
fn rail_text(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32, rows: usize) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat { font_id: font, color, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w,
        max_rows: rows,
        break_anywhere: false,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// The card that names a turn while its tick is under the pointer.
///
/// Opaque and bordered like every other card in the window, and it opens to the **left** of the
/// rail: the rail lives in the margin, so the card grows back over the text, which is the only
/// direction with room. Clamped to the viewport on all four sides — a tick near the last line must
/// not push its card under the composer.
fn rail_card(ui: &Ui, x: f32, cy: f32, title: &str, meta: &str, bounds: Rect, alpha: f32) {
    let pad = 13.0;
    let w = 268.0_f32.min((bounds.width() - 32.0).max(140.0));
    let inner_w = w - pad * 2.0;
    let head = rail_text(ui, title, theme::body_font(13.5), P::text().gamma_multiply(alpha), inner_w, 2);
    let foot = elide(ui, meta, theme::body_font(11.5), P::text_faint().gamma_multiply(alpha), inner_w);
    let gap = 5.0;
    let h = pad * 2.0 + head.size().y + gap + foot.size().y;
    let mut rect = Rect::from_min_size(Pos2::new(x - RAIL_FROM_BAR - w, cy - h * 0.5), Vec2::new(w, h));
    // Nudge, never squash: the top edge wins first, then the bottom, so a card taller than the
    // viewport still starts at the top rather than being pushed above it. The top limit is the
    // title bar's bottom edge for the same reason the track starts there — a card clamped to the
    // viewport's own top would slide under the caption buttons.
    let ceiling = bounds.top().max(TITLEBAR_H) + 8.0;
    let dy = (ceiling - rect.top()).max(0.0) + (bounds.bottom() - 8.0 - rect.bottom()).min(0.0);
    rect = rect.translate(Vec2::new(0.0, dy));
    if rect.left() < bounds.left() + 8.0 {
        rect = rect.translate(Vec2::new(bounds.left() + 8.0 - rect.left(), 0.0));
    }
    let p = ui.painter();
    p.rect_filled(
        rect.translate(Vec2::new(0.0, 2.0)),
        CornerRadius::same(theme::R2),
        P::shade(0.05 * alpha),
    );
    p.rect_filled(rect, CornerRadius::same(theme::R2), P::surface().gamma_multiply(alpha));
    p.rect_stroke(
        rect,
        CornerRadius::same(theme::R2),
        Stroke::new(1.0, P::border().gamma_multiply(alpha)),
        egui::StrokeKind::Inside,
    );
    let at = Pos2::new(rect.left() + pad, rect.top() + pad);
    // The title may hold one unbreakable run (see `rail_text`), so it is clipped to the card rather
    // than allowed to paint over the margin.
    let p = p.with_clip_rect(rect.shrink(1.0));
    p.galley(at, head.clone(), P::text());
    p.galley(
        Pos2::new(at.x, at.y + head.size().y + gap),
        foot,
        P::text_faint(),
    );
}

/// One tick per question on the right edge of the column — the conversation as a map.
///
/// The track starts **below the title bar**, not at the viewport's top edge: the bar is a
/// `Foreground` area laid over the window's first 52 points while the central panel (and with it the
/// scroll area) begins at 0 behind it, so a track measured from `bounds.top()` puts its first tick
/// under the caption buttons — measured, at y = 39.7, invisible.
///
/// Returns the content y the reader asked to be taken to (a click on a tick, or `Ctrl+↑` / `Ctrl+↓`,
/// which walk the same list); `None` when nothing was asked. `alpha` is the slide-in progress, so
/// the rail fades and drifts in from the edge instead of appearing.
fn conversation_rail(
    ui: &mut Ui,
    items: &[Item],
    qs: &[(usize, f32)],
    x: f32,
    bounds: Rect,
    offset: f32,
    alpha: f32,
) -> Option<f32> {
    let top = bounds.top().max(TITLEBAR_H) + RAIL_INSET;
    let len = bounds.bottom() - RAIL_INSET - top;
    if len < 48.0 || qs.is_empty() {
        return None;
    }
    let qy: Vec<f32> = qs.iter().map(|(_, y)| *y).collect();
    let ys = rail_tick_ys(qs.len(), top, len);
    let cur = rail_current(&qy, offset);
    // Below half-opacity the rail is on its way in or out; it must not be clickable while it cannot
    // really be seen.
    let live = alpha > 0.5;
    let mut asked: Option<f32> = None;
    let mut hot: Option<usize> = None;
    for (i, cy) in ys.iter().copied().enumerate() {
        let is_cur = i == cur;
        let mut hovered = false;
        if live {
            // A 24×12 target around a 3-point mark: the tick is the thing you aim at, not the thing
            // the pointer has to hit.
            let hit = Rect::from_center_size(Pos2::new(x, cy), Vec2::new(24.0, 12.0));
            let r = ui
                .interact(hit, ui.id().with(("aox-rail", i)), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            hovered = r.hovered();
            if hovered {
                hot = Some(i);
            }
            if r.clicked() {
                asked = Some(qs[i].1);
            }
        }
        // Centred on the rail's axis, not right-aligned: a mark that grows only to the left made the
        // current turn sit off-axis from its neighbours.
        let cx = x + (1.0 - alpha) * 10.0;
        if is_cur {
            ui.painter().rect_stroke(
                Rect::from_center_size(Pos2::new(cx, cy), Vec2::new(RAIL_W_CUR, RAIL_BOX_H)),
                CornerRadius::same(2),
                Stroke::new(1.0, P::text().gamma_multiply(alpha)),
                egui::StrokeKind::Inside,
            );
        } else {
            let w = if hovered { RAIL_W_CUR } else { RAIL_W };
            let color = if hovered { P::text() } else { P::text_faint() };
            ui.painter().rect_filled(
                Rect::from_center_size(Pos2::new(cx, cy), Vec2::new(w, RAIL_H)),
                CornerRadius::same(2),
                color.gamma_multiply(alpha),
            );
        }
    }

    if let Some(i) = hot {
        let (q, _) = qs[i];
        let title = match &items[q] {
            Item::User(t, _) => rail_question_text(t),
            _ => String::new(),
        };
        let (calls, secs) = rail_turn(items, q);
        let mut meta = format!("#{} / {}", i + 1, qs.len());
        if calls > 0 {
            meta.push_str(&format!(" · {}", aox::tf!("{} calls", calls)));
        }
        if let Some(s) = secs {
            meta.push_str(&format!(" · {s:.1}s"));
        }
        rail_card(ui, x, ys[i], &title, &meta, bounds, alpha);
    }

    // `Ctrl+↑` / `Ctrl+↓`: the same walk, without having to find a 3-point mark first. Skipped while
    // anything holds keyboard focus — inside the input box these keys move the caret.
    if live && qs.len() >= 2 && ui.memory(|m| m.focused().is_none()) {
        let step = ui.input(|i| {
            if !i.modifiers.command {
                0
            } else if i.key_pressed(egui::Key::ArrowUp) {
                -1
            } else if i.key_pressed(egui::Key::ArrowDown) {
                1
            } else {
                0
            }
        });
        if step != 0 {
            let t = (cur as i32 + step).clamp(0, qs.len() as i32 - 1) as usize;
            if t != cur {
                asked = Some(qs[t].1);
            }
        }
    }

    asked
}

// ────────────── settings dialog ──────────────

impl App {
    /// `composer_top` is the input card's **actual** top edge for this frame. The viewport stops
    /// there instead of running to the window's bottom edge: the card is a floating `Area`, so
    /// without this boundary the conversation keeps painting underneath it and the last lines (and
    /// the per-turn cache-hit block) are permanently half-covered.
    pub(super) fn transcript(&mut self, ui: &mut Ui, composer_top: f32) {
        let avail = ui.available_width();
        let (inner, pad) = column(avail);
        let viewport_top = ui.cursor().top();
        let viewport_h = (composer_top - viewport_top).max(0.0);
        // What a **relative** path in a reply is relative to, for the two leaf consumers that are
        // several layers of painting code away from here (`markdown::scan`'s inline-code paths and
        // the file name on a tool-call row). The workspace root, not the app root: that is the
        // directory the agent actually writes into, and the one the user thinks in.
        crate::open::set_workspace_root(
            &self.ws.view().and_then(|v| v.root.clone()).unwrap_or_else(aox::paths::app_root),
        );
        // Jump plumbing (`App::jump`), all read or written through **locals**: the item loop holds
        // `&mut self.items`, and reaching for a field from inside it while the closure already
        // borrows most of `self` is exactly the kind of thing that stops compiling later.
        //
        // - `jump_to` is this frame's assignment, copied out before the builder;
        // - `last_reply` is the most recent assistant block, for the usage line's chip: the flag says
        //   its first line is off-screen, so the chip only appears where it can do something;
        // - `questions` is every user message with its content y — that is the whole input the rail
        //   on the right edge needs, and it is only knowable *here*, while the layout is in hand;
        // - `jump` is what a click this frame asks for, applied to `self` after `show` returns.
        let jump_to = self.jump;
        // This frame's scroll offset, i.e. the value the content is being painted at: `begin()`
        // loaded it from the state saved at the end of the previous frame, which is exactly what
        // `last_scroll_y` holds. Turning a screen y into a content y needs it.
        let offset_now = self.last_scroll_y;
        let mut last_reply: Option<(f32, bool)> = None;
        let mut questions: Vec<(usize, f32)> = Vec::new();
        let mut jump: Option<f32> = None;
        // One frame of `vertical_scroll_offset` is what performs a jump. It replaces the offset
        // **before** layout, so the frame that jumps already paints in the new place.
        //
        // The alternative — `ui.scroll_to_rect` from inside the content — is what the first version
        // used, and it did nothing: it only records a *target* that egui animates in from the
        // following frame (`ScrollArea::animated` is on by default), and in between the sticky
        // correction runs with its latch still set and drags the view back to the bottom. Verified
        // by instrumenting the click: the target was accepted, the offset never moved.
        let mut area = ScrollArea::vertical()
            .max_height(viewport_h)
            .min_scrolled_height(viewport_h)
            .auto_shrink([false, false]);
        area = match jump_to {
            Some(y) => area.vertical_scroll_offset(y),
            None => area.stick_to_bottom(self.follow || self.force_follow),
        };
        let out = area
            .show(ui, |ui| {
                // Top padding = the old top bar's 50 + the original 26. With the bar gone the
                // buttons float above the content, and without the padding the first line of text
                // butts right up under them.
                ui.add_space(76.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_max_width(inner);
                        ui.set_min_width(inner);
                        // The measure's left/right bounds: width = inner, wrapping automatically at
                        // the right edge.
                        //
                        // egui's `Ui::wrap_mode()` returns `Extend` inside a "non-wrapping horizontal
                        // layout" and `Label` inherits that by default -- so long text runs to the
                        // end of the line and gets clipped past the right edge. Bubbles, list items
                        // and tool-call rows are all such horizontal layouts internally, so it is
                        // nailed down once at the column root and every descendant Ui (including
                        // inside a Frame) inherits `Wrap`.
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                        // Until the real conversation starts, show the first screen (there may
                        // already be loading / error notices in `items` at this point). The same
                        // predicate decides whether the background gets painted, in `App::update`
                        // -- one function, so "the image is up" and "the first screen is up" cannot
                        // drift apart.
                        if self.on_empty_state() {
                            self.empty_state(ui);
                        }
                        // Jump plumbing (`App::jump`), all read or written through **locals**: the
                        // item loop holds `&mut self.items`, and a disjoint field is easy to get
                        // wrong in a closure that already borrows most of `self`.
                        //
                        // - `view_top` is the viewport's top edge in screen space (the ScrollArea
                        //   starts exactly at the column's cursor, and its height is `viewport_h`);
                        // - `last_reply` / `jump` are filled in by the loop below.
                        let view_top = viewport_top;
                        // Image rendering has to consult the texture cache. `self.items` and
                        // `self.tex` are **two unrelated fields**, so take a reference that borrows
                        // only `tex` first, and borrowing `items` in the loop no longer conflicts
                        // (borrowing all of `&mut self` would not work).
                        let tex = &self.tex;
                        for (i, item) in self.items.iter_mut().enumerate() {
                            // The item's own top edge, taken before anything is drawn: the vertical
                            // layout's cursor is where this item starts.
                            let top = ui.cursor().min.y;
                            match item {
                                Item::User(t, files) => {
                                    // The rail's only input: a question's head, in content space.
                                    questions.push((i, top - view_top + offset_now));
                                    // A new question starts a new turn, so the reply the usage line
                                    // would point at is no longer "the one above it": a turn that dies
                                    // before writing any answer (an early failure, an abort) still
                                    // emits its usage row, and without this reset that row's chip
                                    // would jump into the *previous* turn's reply.
                                    last_reply = None;
                                    user_bubble(ui, t, files, inner * 0.80)
                                }
                                Item::Assistant(t) => {
                                    assistant_block(ui, t, inner);
                                    // The reply's first line in the scroll area's **own** coordinates
                                    // (the origin of the content, not of the window): recorded while
                                    // the layout is in hand, so a jump later never has to guess where
                                    // a heading, a table or a code card put it.
                                    let y = top - view_top + offset_now;
                                    last_reply = Some((y, top < view_top - 1.0));
                                }
                                Item::Process(p) => process_block(ui, p, inner, tex),
                                Item::Note(k, t) => note_row(ui, *k, t),
                                Item::Images(paths) => images_block(ui, paths, inner, tex),
                                Item::Usage(t) => {
                                    // The chip belongs to the reply above it, and only when that
                                    // reply's head is off-screen — otherwise "jump to the start" is a
                                    // button that does nothing visible.
                                    let back = last_reply.and_then(|(y, above)| above.then_some(y));
                                    if usage_row(ui, t, back.is_some()) {
                                        jump = back;
                                    }
                                }
                                Item::Error(t) => error_card(ui, t, inner),
                            }
                        }
                        if self.live.is_some() {
                            self.live_block(ui, inner);
                        }
                        // The viewport already ends above the card; this is breathing room for its
                        // top edge and shadow, not a second copy of the composer's height.
                        ui.add_space(TRANSCRIPT_BOTTOM_GAP);
                        // `stick_to_bottom` **cannot** put a view that has scrolled up back at the
                        // bottom: egui only honours the sticky flag while its own
                        // `scroll_stuck_to_end` latch is set, and that latch drops the moment the
                        // user scrolls away from the end (`scroll_area.rs`, "Once unstuck (false)
                        // it remains false until the scroll touches the end position"). So after
                        // "scroll up, then send a message" the row was appended below the viewport
                        // and nothing followed it — reported 2026-10-28.
                        //
                        // Asking for the bottom explicitly is what actually moves it. `Some(BOTTOM)`
                        // rather than `None`: `None` only scrolls "enough to be visible", which for
                        // a viewport-tall block means the top of it, not the end of the column.
                        if self.force_follow && self.jump.is_none() {
                            ui.scroll_to_rect(ui.min_rect(), Some(Align::BOTTOM));
                        }
                    });
                });
            });
        let max = (out.content_size.y - out.inner_rect.height()).max(0.0);
        let y = out.state.offset.y;
        // Did the user pull **up** this frame? A decrease in the offset can only come from them
        // (our own `scroll_to_rect` and the sticky growth both push it down), and it has to be able
        // to break the lock below -- otherwise a lock that never reaches the bottom (a wheel event
        // racing the animation, a content height that keeps changing under it) would fight the
        // wheel every frame and the list would feel frozen.
        let pulled_up = y < self.last_scroll_y - 1.0;
        self.last_scroll_y = y;
        let at_bottom = y >= max - 48.0;
        if self.force_follow {
            self.follow = true;
            if at_bottom || pulled_up {
                self.force_follow = false;
            }
        } else {
            self.follow = at_bottom;
        }

        // The jump asked for on the **previous** frame has just been carried out by the builder
        // above, so it is spent.
        if jump_to.is_some() {
            self.jump = None;
        }

        // ── the conversation rail ───────────────────────────────────────────────
        // Drawn **after** the scroll area, so it paints over the text rather than under it, and so
        // the card it opens is never buried by a paragraph. `out.inner_rect` (not the panel's rect)
        // is the viewport: it already excludes the scroll bar, which is what keeps the ticks from
        // sitting on the strip the pointer uses to drag the view.
        //
        // Where it goes: the **middle of the gutter** between the text and the window's right edge,
        // so it moves with the window instead of tracking the column at a fixed offset. Neither edge
        // works on its own — glued to the column it looked cramped at 1080 while half the margin sat
        // empty on its right, and pinned to the window edge it would read as chrome on a 1920 screen.
        // Mid-gutter is the one place that is "beside the text" at every width. The clamp keeps it
        // clear of the scroll bar, and `fits` gives up when the margin is too thin to hold it without
        // touching the text.
        let column_right = out.inner_rect.left() + pad + inner;
        let rail_x = ((column_right + out.inner_rect.right()) * 0.5)
            .min(out.inner_rect.right() - RAIL_FROM_BAR);
        let rail_fits = rail_x - RAIL_W_CUR * 0.5 >= column_right + 6.0;
        let want = if rail_fits && questions.len() >= RAIL_MIN { 1.0 } else { 0.0 };
        // Stepped by frame, the same way `hist_t` is: a conversation that grows its third question
        // while the reader watches has to continue from wherever the rail already is, and a window
        // narrowed until the margin disappears has to slide it back out.
        if (self.rail_t - want).abs() > f32::EPSILON {
            let step = (ui.input(|i| i.stable_dt).min(0.1) / RAIL_SECS).max(0.0);
            self.rail_t = if self.rail_t < want {
                (self.rail_t + step).min(want)
            } else {
                (self.rail_t - step).max(want)
            };
            ui.ctx().request_repaint();
        }
        let rail_jump = if self.rail_t > 0.0 {
            conversation_rail(
                ui,
                &self.items,
                &questions,
                rail_x,
                out.inner_rect,
                offset_now,
                crate::app::ease_out_cubic(self.rail_t),
            )
        } else {
            None
        };

        // What the controls ask for this frame. The move itself happens on the **next** frame:
        // whether the view follows the end is decided before the content is laid out, and a control
        // clicked while sitting at the bottom cannot be honoured in that same frame.
        if let Some(y) = rail_jump.or(jump) {
            // `JUMP_MARGIN` of breathing room above the first line, and never a negative offset.
            self.jump = Some((y - JUMP_MARGIN).max(0.0));
            self.follow = false;
            self.force_follow = false;
            // That next frame has to actually happen. An idle egui window only repaints when
            // something asks it to, and a click asks for its own frame and nothing more — which is
            // exactly why the first version of this button looked dead: the request was queued and
            // then sat there until the pointer happened to move.
            ui.ctx().request_repaint();
        }
    }

    /// Is the first screen (nothing said yet) the thing on show?
    ///
    /// This decides two things that must agree: whether the transcript draws the first screen, and
    /// whether the background image is painted behind it. Notices that arrived before any message
    /// (a load failure, "model not ready") do **not** end the first screen -- they are the first
    /// screen's content, not a conversation.
    pub(super) fn on_empty_state(&self) -> bool {
        self.live.is_none()
            && !self
                .items
                .iter()
                .any(|i| matches!(i, Item::User(..) | Item::Assistant(_)))
    }

    pub(super) fn empty_state(&mut self, ui: &mut Ui) {
        // What used to be here was the illustration itself. It is now a **full-bleed background**
        // painted behind the whole column (`App::update` -> `brand::show_background`), so there is
        // nothing left to reserve space for or centre: the empty state is just the "model not
        // ready" card, or nothing at all. The old "Hi there" line and the preset examples beneath
        // it were removed at the user's request (2026-10-06) -- the empty state no longer presumes
        // to think of what the user might say.
        //
        // A background that failed to load is reported *here* rather than where it was painted:
        // this is where there is room for a line of text in the column, and "a file in the folder
        // isn't really a PNG" is otherwise invisible -- the first screen would simply be plain, at
        // random, which reads as "the feature is flaky".
        if self.log_error.is_none() {
            if let Some(e) = self.brand_err.clone() {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(aox::tf!("The background image could not be loaded: {}", e))
                            .color(P::text_faint())
                            .size(13.0),
                    );
                });
            }
            return;
        }

        // With an error card (model not configured) the card is **not** centred: the single most
        // important thing on screen then is that error, and centring it would push it down out of
        // sight on a short window. (This 8.0 is what the illustrated version used in the same case,
        // so the card has not moved.)
        ui.add_space(8.0);
        ui.vertical_centered(|ui| {
            if let Some(err) = &self.log_error {
                ui.add_space(38.0);
                let w = 640.0_f32.min(ui.available_width() - 8.0);
                Frame::NONE
                    .fill(P::surface())
                    .corner_radius(CornerRadius::same(theme::card_radius()))
                    .stroke(Stroke::new(1.0, P::edge_raised()))
                    .inner_margin(Margin::symmetric(22, 18))
                    .show(ui, |ui| {
                        ui.set_max_width(w);
                        Frame::NONE
                            .fill(P::reverse_bg())
                            .corner_radius(CornerRadius::same(theme::R0))
                            .inner_margin(Margin::symmetric(11, 4))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(aox::i18n::t("Model not ready"))
                                        .color(P::on_reverse())
                                        .size(14.0),
                                );
                            });
                        ui.add_space(10.0);
                        ui.label(RichText::new(err).color(P::text_dim()).size(14.5));
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(aox::tf!("Config file: {}", self.config_path))
                                .color(P::text_faint())
                                .size(13.5),
                        );
                        ui.label(
                            RichText::new(aox::i18n::t("Fill in base_url / api_key under [provider], then restart."))
                                .color(P::text_faint())
                                .size(13.5),
                        );
                    });
            }
        });
    }

    pub(super) fn live_block(&mut self, ui: &mut Ui, width: f32) {
        let _ = width;
        // Taken off `self` before the live state is borrowed below: a call row can now be unfolded,
        // which needs the step list `&mut`, and `self.now()` would be a second, conflicting borrow of
        // the whole struct.
        let spin = self.spin;
        let now = self.now();
        // A different field from `live`, so the two borrows coexist: the rows need the step list
        // mutably and the images need the decoded textures.
        let tex = &self.tex;
        let Some(l) = self.live.as_mut() else { return };
        // The live area is cut by **stage**: a stretch of call stream -> one interim reply ->
        // another stretch of call stream -> ...
        // (user request, 2026-10-05: previously all the calls were crammed at the top and all
        // the replies in the middle, so you couldn't tell which steps belonged to which stage.)
        //
        // `cuts[i]` = the number of steps already run when interim reply i was spoken, so call
        // stretch i is `steps[cuts[i-1] .. cuts[i]]`, and the last one runs to `steps.len()`.
        for seg in 0..=l.interim.len() {
            let from = if seg == 0 { 0 } else { l.cuts.get(seg - 1).copied().unwrap_or(0) };
            let to = l.cuts.get(seg).copied().unwrap_or(l.steps.len()).min(l.steps.len());
            if from < to {
                calls_card(ui, &mut l.steps[from..to], tex);
                ui.add_space(14.0);
            }
            if let Some(say) = l.interim.get(seg) {
                live_plain_text(ui, say, P::text_dim());
                ui.add_space(14.0);
            }
        }
        // The sentence being typed goes last: it doesn't belong to any stage yet (only once
        // it's finished do you know whether more calls follow).
        if !l.streaming.trim().is_empty() {
            live_plain_text(ui, &l.streaming, P::text());
            ui.add_space(14.0);
        }

        // The spinner, the "Thinking" line and the reasoning band all belong to **actual** thinking.
        //
        // ① A model that sends no `reasoning_content` sat behind a spinning ring and the word
        //    "Thinking" for the whole turn — an animation claiming work that never happened.
        // ② Once the answer starts typing, they are **stale**: the band still showed the tail of the
        //    reasoning (the last thoughts before the model decided to write) underneath the answer
        //    that is already being printed — reported 2026-10-30 with a screenshot: one leftover
        //    thinking line plus "Thinking 16.4s ~2,637 tok" below a finished-looking reply.
        // ③ Both are the **last** things in the live area, so every delta of a fast answer pushed
        //    them one line further down: they shook for the length of the reply.
        //
        // So the band (and its caption) is drawn only while the model is **thinking and not yet
        // writing**: there is reasoning text and no body text on screen. The moment the first body
        // delta lands, thinking leaves the stage — the answer is the thing to read then. A status
        // that is not the default still shows its head regardless ("Running shell_exec",
        // "Retrying in 3 s" are facts, not decoration); those never coincide with a body stream,
        // because a retry clears it and a running call has narration, not typing.
        let reasoning = has_reasoning(&l.reasoning);
        let thinking = thinking_band_visible(reasoning, !l.streaming.trim().is_empty());
        let show_head = thinking_head_visible(&l.status, thinking);
        // waiting animation + status
        ui.horizontal(|ui| {
            if show_head {
                spinner_dot(ui, spin, P::ink());
                ui.add_space(8.0);
                ui.label(
                    RichText::new(if l.status.is_empty() { aox::i18n::t("Thinking") } else { &l.status })
                        .color(P::text_dim())
                        .size(15.0),
                );
                ui.add_space(8.0);
            }
            // The **call's** clock while a call is running, the turn's otherwise. A running call is
            // the one case where "how long has this been going" has an answer the user can act on,
            // and it needs its ceiling right next to it: `0:47 / 2:00` says "fine", `1:58 / 2:00`
            // says "about to be cut off".
            match &l.call {
                Some(c) => {
                    ui.label(
                        RichText::new(span(now - c.started, c.cap as f32))
                            .color(P::text_faint())
                            .size(13.0),
                    );
                    if let Some(e) = c.expect {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(aox::tf!("expected ~{}", clock(e as f32)))
                                .color(P::text_faint())
                                .size(13.0),
                        );
                    }
                }
                None => {
                    ui.label(
                        RichText::new(format!("{:.1}s", (now - l.turn_base).max(0.0)))
                            .color(P::text_faint())
                            .size(13.0),
                    );
                }
            }
            // The output token count scrolls live. The `~` is because the server hasn't reported
            // during streaming and this value is estimated from characters; once the turn ends the
            // exact total takes over.
            let est = l.out.tokens();
            if est > 0 {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("~{} tok", thousands(est)))
                        .color(P::text_faint())
                        .size(13.0),
                );
            }
        });

        // The reasoning box, drawn **only when there is reasoning to read** (user request,
        // 2026-10-25).
        //
        // It was built the other way round on 2026-10-05 -- fixed height, always present -- to stop
        // the block bouncing: it used to grow from one line to four while streaming and then vanish
        // at the end of the turn. The price of that fix is what this one reverses: a model that does
        // not reason (or a provider that sends no `reasoning_content`) left an empty rounded
        // rectangle on screen for the whole turn, looking like "thinking" while nothing was being
        // thought.
        //
        // The old complaint stays solved: the box appears only on the first delta of a turn that
        // actually reasons, it never grows past four lines (the text is clipped inside a fixed
        // height below), and at the end of the turn the whole live block is replaced by the process
        // card, which carries the reasoning in its own place. So the height still moves at most
        // once, and only when there is something to show.
        // The reasoning band is a **transparent** enclosure (2026-10-30): the card (`code_bg` fill
        // plus a 1px `border` stroke) is gone, and only the text is left. Everything else is
        // untouched on purpose -- the band's height (4 lines + 26), the 16/13 inner inset (so the
        // text keeps its indent and its left/right width), the clipping, the font and the faint
        // colour all stay exactly as they were, and so does the 12/16 spacing around it. What is
        // gone is only the box drawn *behind* the words.
        const THINK_LINES: usize = 4;
        const THINK_LINE_H: f32 = 20.0;
        if thinking {
            ui.add_space(12.0);
            let w = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(w, THINK_LINE_H * THINK_LINES as f32 + 26.0),
                Sense::hover(),
            );
            let inner = rect.shrink2(Vec2::new(16.0, 13.0));
            let tail = tail_lines(&l.reasoning, THINK_LINES);
            // Clip first, then draw: text past four lines gets cut off rather than pushing the
            // band taller.
            let p = ui.painter().with_clip_rect(inner);
            let g = p.layout(tail, theme::body_font(14.5), P::text_faint(), inner.width());
            p.galley(inner.left_top(), g, P::text_faint());
            ui.add_space(16.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The usage line keeps the **same height** whether or not the jump chip is on it.
    ///
    /// The chip's flag is "the reply's head is above the viewport", which is a property of the
    /// scroll position — so a row that grew or shrank with it would re-lay the whole transcript out
    /// every time a long answer's head crossed the top edge: the content would jump under the
    /// pointer, and the very reply the chip belongs to would flicker. Its slot is therefore reserved
    /// unconditionally (see `composer::usage_row`), and this pins that.
    #[test]
    fn the_jump_chip_never_changes_the_usage_row_height() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let text = "#1 · 3 calls · Input 11,798 · Output 7,754 · Total 19,552 · Reasoning 3,723 · Cache hit 11,648 · Cache miss 150";
        let mut h = [0.0_f32; 2];
        for (k, back) in [false, true].into_iter().enumerate() {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 400.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = usage_row(ui, text, back);
                    h[k] = ui.min_rect().height();
                });
            });
        }
        assert!(
            (h[0] - h[1]).abs() < 0.01,
            "the row must not change height with the chip: without = {}, with = {}",
            h[0],
            h[1]
        );
    }

    /// History replay must hide the host's attachment sentence and put each picture under the
    /// `attach_file` call that produced it.
    ///
    /// Two calls, two pictures: matching only by tool name would put both under the last row. The
    /// exact `path` argument is what keeps them apart, and the absence of a `Note` is the other half
    /// of the user-visible fix (the host sentence is protocol for the model, not conversation text).
    #[test]
    fn replayed_tool_images_sit_under_their_own_call_and_hide_the_host_hint() {
        use aox::message::{FunctionCall, Message, ToolCall};

        let d = std::env::temp_dir().join("aox-gui-images-under-call");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("a.png");
        let b = d.join("b.png");
        for p in [&a, &b] {
            std::fs::write(p, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        }
        let call = |id: &str, path: &Path| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "attach_file".into(),
                arguments: serde_json::json!({ "path": path }).to_string(),
            },
        };
        let host_text = aox::tf!(
            "{} the files below were produced by the tool call just now.",
            aox::history::HOST_PREFIX
        );
        let host = Message::user_with_files(
            &host_text,
            &[
                aox::attach::Attachment::probe(a.clone()).unwrap(),
                aox::attach::Attachment::probe(b.clone()).unwrap(),
            ],
        )
        .unwrap();
        let loaded = vec![
            aox::history::Loaded {
                msg: Message::assistant(None, None, vec![call("c1", &a), call("c2", &b)]),
                is_error: None,
            },
            aox::history::Loaded {
                msg: Message::tool_result("c1", "attach_file", "attached a".into()),
                is_error: None,
            },
            aox::history::Loaded {
                msg: Message::tool_result("c2", "attach_file", "attached b".into()),
                is_error: None,
            },
            aox::history::Loaded { msg: host, is_error: None },
            aox::history::Loaded {
                msg: Message::assistant(Some("done".into()), None, vec![]),
                is_error: None,
            },
        ];

        let items = items_from_history(&loaded);
        assert!(
            !items.iter().any(|it| matches!(it, Item::Note(_, t) if t.contains("(host hint)"))),
            "the protocol sentence must not be drawn as a transcript note"
        );
        assert!(
            !items.iter().any(|it| matches!(it, Item::Images(_))),
            "tool images must not become a separate block at the top of a message"
        );
        let p = items
            .iter()
            .find_map(|it| match it {
                Item::Process(p) => Some(p),
                _ => None,
            })
            .expect("the two calls form one process");
        assert_eq!(p.steps[0].images, vec![a], "first call keeps its own picture");
        assert_eq!(p.steps[1].images, vec![b], "second call keeps its own picture");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// The live reasoning panel is drawn only when there is a sentence in it.
    ///
    /// Reported 2026-10-25: a model that does not reason (or a provider that sends no
    /// `reasoning_content`) left an empty rounded card under the status row for the whole turn. The
    /// box used to be unconditional **on purpose** (2026-10-05, "it keeps growing and shrinking, so
    /// the whole screen jumps up and down"), so this pins the new rule from both sides: blank hides
    /// it, one word shows it — and a stray leading newline does not get to hide real text.
    #[test]
    fn a_blank_reasoning_field_draws_no_card() {
        assert!(!has_reasoning(""), "an absent field must not draw a card");
        assert!(!has_reasoning("\n"), "providers that open with a bare newline must not draw one");
        assert!(!has_reasoning(" \n\t\n"), "a field of nothing but whitespace is still nothing");
        assert!(has_reasoning("Counting files by type"), "one sentence is enough to earn the card");
        assert!(has_reasoning("\n\n先看一下目录结构"), "leading newlines must not hide real text");
    }

    /// The spinner-and-"Thinking" head is a claim about the model, so it may only appear when that
    /// claim is true — or when the line under it says something else worth reading.
    ///
    /// Reported 2026-10-30: a model that returns no reasoning sat behind a spinning ring and the
    /// word "Thinking" for the whole turn, and both shook violently while the final answer streamed
    /// in (they are the last two things in the live area, so every delta pushed them down a line).
    #[test]
    fn the_thinking_head_needs_either_a_band_or_a_real_status() {
        let thinking = aox::i18n::t("Thinking");
        assert!(
            !thinking_head_visible(thinking, false),
            "no band + the default line = no head at all"
        );
        assert!(thinking_head_visible(thinking, true), "a visible band earns its caption");
        assert!(
            thinking_head_visible("Running shell_exec", false),
            "a running call must still say what it is doing"
        );
        assert!(
            thinking_head_visible("Retrying in 3.0 s", false),
            "a backoff is exactly the moment the head has to stay"
        );
        assert!(!thinking_head_visible("", false), "an empty status is not a claim either");
    }

    /// The band itself steps aside once the answer starts typing.
    ///
    /// Reported 2026-10-30 (screenshot): after the final answer had printed, one leftover thinking
    /// line was still hanging under it together with the "Thinking" head. The tail it showed was the
    /// reasoning *before* the model started writing — stale by then, and it is the last block in the
    /// live area, so it also shook on every delta.
    #[test]
    fn the_reasoning_band_leaves_the_stage_when_the_answer_types() {
        assert!(thinking_band_visible(true, false), "thinking, nothing written yet = it belongs");
        assert!(
            !thinking_band_visible(true, true),
            "the first body delta retires the band: the answer is what to read now"
        );
        assert!(!thinking_band_visible(false, false), "no reasoning, no band");
        assert!(!thinking_band_visible(false, true), "still no reasoning, still no band");
    }

    // ── the conversation rail ────────────────────────────────────────────────

    fn step(name: &str) -> StepView {
        StepView {
            name: name.into(),
            plugin: None,
            purpose: None,
            subject: None,
            args: "{}".into(),
            result: None,
            diff: None,
            diff_detail: None,
            expanded: false,
            images: Vec::new(),
        }
    }

    fn proc(calls: usize, elapsed: Option<f32>) -> Item {
        Item::Process(Process {
            steps: (0..calls).map(|_| step("bash")).collect(),
            interim: Vec::new(),
            reasoning: String::new(),
            elapsed,
            expanded: false,
            aborted: false,
        })
    }

    /// The track is a miniature of the document, not a ladder.
    ///
    /// Even spacing would say "three questions, three equal chunks" about a conversation whose
    /// third reply is ten times the first — which is exactly the case the rail exists to answer, so
    /// A list, not a minimap: every gap is the same, and the list is centred.
    ///
    /// Two earlier shapes were proportional to the conversation, and both were rejected on sight —
    /// first because three ticks scattered over half a page of margin ("太丑了"), then because the
    /// gaps still came out unequal ("间距还是不一样，应该是等间距的"). Equal rows is the only shape
    /// that reads as an index of turns rather than as a broken ruler.
    #[test]
    fn rail_ticks_are_evenly_spaced_and_centred() {
        let len = 400.0;
        let ys = rail_tick_ys(3, 0.0, len);
        assert_eq!(ys, vec![166.0, 200.0, 234.0], "one RAIL_STEP apart, centred on 200");
        assert!(
            (ys[1] - ys[0] - (ys[2] - ys[1])).abs() < 0.01,
            "equal gaps or it is not a list: {ys:?}"
        );
        // Nine questions still centre, and still step by the full pitch.
        let ys = rail_tick_ys(9, 0.0, len);
        assert!((ys[0] + ys[8]) * 0.5 - len * 0.5 < 0.01, "centred: {ys:?}");
        assert!((ys[1] - ys[0] - 34.0).abs() < 0.01, "full pitch while it fits: {ys:?}");
    }

    /// A long conversation packs tighter instead of running out of the track.
    #[test]
    fn rail_ticks_compress_when_the_list_is_longer_than_the_track() {
        let len = 100.0;
        let ys = rail_tick_ys(40, 0.0, len);
        assert_eq!(ys.len(), 40);
        assert!((ys[0] - 0.0).abs() < 0.01 && (ys[39] - len).abs() < 0.01, "fills the track: {ys:?}");
        assert!(ys.windows(2).all(|w| w[1] > w[0]), "still in order and distinct");
        // Degenerate inputs must not divide by zero or panic.
        assert!(rail_tick_ys(0, 0.0, 100.0).is_empty(), "no questions, no ticks");
        assert_eq!(rail_tick_ys(1, 0.0, 100.0), vec![50.0], "one question sits in the middle");
        assert_eq!(rail_tick_ys(2, 10.0, 20.0), vec![10.0, 30.0], "two: one step, still centred");
    }

    /// Which tick is the filled one: the question whose head is at or above the top edge.
    #[test]
    fn the_rail_marks_the_question_you_are_reading() {
        let qy = [76.0, 900.0, 2400.0];
        assert_eq!(rail_current(&qy, 0.0), 0, "at the very top, the first question is the one on screen");
        assert_eq!(rail_current(&qy, 300.0), 0, "inside the first reply");
        assert_eq!(rail_current(&qy, 900.0), 1, "the head of the second question is exactly at the top");
        assert_eq!(rail_current(&qy, 2300.0), 1, "still inside the second turn's answer");
        assert_eq!(rail_current(&qy, 5000.0), 2, "past every question = the last one");
    }

    /// The card's title is one flattened run, capped — a bubble may start with an indented block.
    #[test]
    fn the_rail_card_flattens_a_question_into_one_run() {
        assert_eq!(rail_question_text("  where does\n\n\tthe config  live?  "), "where does the config live?");
        assert_eq!(rail_question_text(""), "", "an empty question stays empty");
        let long: String = "字".repeat(400);
        let out = rail_question_text(&long);
        assert_eq!(out.chars().count(), 160, "the cap keeps a pasted log out of the layout pass");
    }

    /// A rail tick reports its own turn, never the next one's work.
    #[test]
    fn a_rail_tick_counts_only_its_own_turn() {
        let items = vec![
            Item::User("first".into(), Vec::new()),
            proc(2, Some(5.0)),
            Item::Assistant("done".into()),
            proc(1, None),
            Item::User("second".into(), Vec::new()),
            proc(3, None),
        ];
        let (calls, secs) = rail_turn(&items, 0);
        assert_eq!(calls, 3, "both process cards of the first turn, and nothing from the second");
        assert_eq!(secs, Some(5.0), "the only card that recorded a duration");
        let (calls, secs) = rail_turn(&items, 4);
        assert_eq!(calls, 3, "the second turn is on its own");
        assert_eq!(secs, None, "history carries no timing, so the card prints no duration");
    }
}
