//! 远程接管锁：桥在跑就锁住整个界面。
//!
//! 桥每 5 秒写一次 `.aox/remote.lock`（带连接者与服务器），这里每 0.5 秒看一次文件。
//! 文件超过 15 秒没更新就当作桥已死——**不能因为一个崩溃的进程把人永久锁在外面**。
//! 「取回控制权」= 删文件；桥发现文件没了会在 1 秒内自己退出。

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
        who: v["who"].as_str().unwrap_or("手机端").to_string(),
        since: v["since"].as_i64().unwrap_or(0),
    })
}

pub fn release() {
    let _ = std::fs::remove_file(path());
    *CACHE.lock().expect("remote lock cache") = Some((Instant::now(), None));
}

/// 画在所有浮层之上：`Modal` 自带遮罩，底层控件收不到点击。
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
                    RichText::new(aox::tf!("已接管 {} 分钟", mins))
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
