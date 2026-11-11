//! Remote takeover lock: while the bridge runs the whole UI is locked; a crash must not lock users out.
//! The bridge writes .aox/remote.lock every 5s (connector + server); this polls it every 0.5s.
//! Stale over 15s means the bridge died; Reclaim control deletes the file and the bridge exits within 1s.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use egui::RichText;

use crate::theme::{self, P};

#[derive(Clone, Debug)]
pub struct Info {
    pub agent: String,
    pub server: String,
    pub who: String,
    pub since: i64,
}

static CACHE: Mutex<Option<(Instant, Option<Info>)>> = Mutex::new(None);

fn path() -> PathBuf {
    aox::paths::app_root().join(".aox").join("remote.lock")
}

pub fn current() -> Option<Info> {
    let mut g = CACHE.lock().expect("remote lock cache");
    if let Some((at, v)) = g.as_ref() {
        if at.elapsed() < Duration::from_millis(500) {
            return v.clone();
        }
    }
    let fresh = read();
    *g = Some((Instant::now(), fresh.clone()));
    fresh
}

fn read() -> Option<Info> {
    let p = path();
    let meta = std::fs::metadata(&p).ok()?;
    let age = meta.modified().ok().and_then(|m| m.elapsed().ok())?;
    if age > Duration::from_secs(15) {
        return None;
    }
    let text = std::fs::read_to_string(&p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(Info {
        agent: v["agent"].as_str().unwrap_or("agent").to_string(),
        server: v["server"].as_str().unwrap_or_default().to_string(),
        who: v["who"].as_str().unwrap_or("Phone").to_string(),
        since: v["since"].as_i64().unwrap_or(0),
    })
}

pub fn release() {
    let _ = std::fs::remove_file(path());
    *CACHE.lock().expect("remote lock cache") = Some((Instant::now(), None));
}

/// Drawn above all overlays: a Modal carries its own scrim, so controls underneath get no clicks.
pub fn takeover(ctx: &egui::Context, info: &Info) {
    egui::Modal::new(egui::Id::new("aox-remote-takeover"))
        .backdrop_color(P::scrim())
        .frame(
            theme::floating(theme::card_radius()).inner_margin(egui::Margin::symmetric(22, 18)),
        )
        .show(ctx, |ui| {
            ui.set_width(370.0);
            ui.label(
                RichText::new(aox::i18n::t("Remote control is active"))
                    .font(theme::serif_font(20.0))
                    .color(P::text()),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("{} · @{}", info.who, info.agent))
                    .color(P::text_dim())
                    .size(14.0),
            );
            ui.add_space(2.0);
            ui.label(RichText::new(&info.server).color(P::text_faint()).size(12.0));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let mins = ((now - info.since) / 60_000).max(0);
            if mins > 0 {
                ui.label(
                    RichText::new(aox::tf!("Under remote control for {} min", mins))
                        .color(P::text_faint())
                        .size(12.0),
                );
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(aox::i18n::t(
                    "This window is locked while the phone is driving. Taking back control disconnects it.",
                ))
                .color(P::text_faint())
                .size(12.5),
            );
            ui.add_space(14.0);
            if ui
                .button(RichText::new(aox::i18n::t("Take back control")).size(14.5))
                .clicked()
            {
                release();
            }
        });
}
