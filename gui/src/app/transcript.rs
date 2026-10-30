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
        let out = ScrollArea::vertical()
            .max_height(viewport_h)
            .min_scrolled_height(viewport_h)
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow || self.force_follow)
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
                        // Image rendering has to consult the texture cache. `self.items` and
                        // `self.tex` are **two unrelated fields**, so take a reference that borrows
                        // only `tex` first, and borrowing `items` in the loop no longer conflicts
                        // (borrowing all of `&mut self` would not work).
                        let tex = &self.tex;
                        for item in &mut self.items {
                            match item {
                                Item::User(t, files) => user_bubble(ui, t, files, inner * 0.80),
                                Item::Assistant(t) => assistant_block(ui, t, inner),
                                Item::Process(p) => process_block(ui, p, inner, tex),
                                Item::Note(k, t) => note_row(ui, *k, t),
                                Item::Images(paths) => images_block(ui, paths, inner, tex),
                                Item::Usage(t) => usage_row(ui, t),
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
                        if self.force_follow {
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
}
