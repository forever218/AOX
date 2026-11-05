//! The `--demo` data: a fake conversation and a fake live turn, so the layout can be looked at
//! without a model.
//!
//! Kept in the build rather than behind `#[cfg(test)]` on purpose: `--demo` is how the interface gets
//! reviewed on a machine with no API key, and it has to exercise the **real** painter -- every row it
//! invents goes through the same code a real turn does.

use super::*;

pub(super) fn seed_demo(app: &mut App) {
    app.model = "deepseek-chat".into();
    app.config_path = "config.toml".into();
    app.plugins = vec!["filestat".to_string()];
    app.status = aox::i18n::t("Ready").into();
    // Demo mode reads history from the real application root too: a preview should look real,
    // and making up a few fake conversations would just make people think the sidebar is
    // ornamental.
    let root = aox::paths::resolve_workplace().0;
    let store = aox::store::default_store(&root, 0);
    // One store, one directory (2026-11-05): `--demo` reads the same conversations the real window
    // does, with no extra wiring.
    app.boot = Some(BootInfo {
        config_path: app.config_path.clone(),
        // The demo data gets a realistic layer chain too, so that row in the "About" window is
        // visible under --demo.
        config_layers: vec!["bundle:core".into(), "config.toml".into()],
        model: app.model.clone(),
        context_window: DEFAULT_CONTEXT_WINDOW,
        provider_ready: true,
        recovery: true,
        // Demo mode is not a fresh install: opening the settings dialog over the preview would hide
        // the very layout `--demo` exists to show.
        first_run: false,
        note: None,
        // Demo mode gets a real path listing as well: the settings page's "System" column is
        // exactly the kind of "one glance is enough" diagnostic data, so previewing it with fake
        // paths is pointless (and the real paths are readable in demo mode anyway).
        paths: BootPaths::new(
            &root,
            std::path::Path::new(&app.config_path),
            &root.join("plugins"),
            store.location().to_path_buf(),
            // `--demo` never loads the config, so the shell row uses the defaults (`[shell] bash =
            // "auto"`). Detection itself only stats files, so this is a real answer for this machine,
            // not a placeholder.
            &aox::config::ShellConfig::default(),
        ),
        store: store.clone(),
    });
    app.store = Some(store);
    app.refresh_history();
    // A plan for the bar above the input: `--demo` is how the interface is verified without a
    // model, and a bar nobody can see in the demo is a bar nobody checks.
    let demo_task = aox::task::TaskState::steps_demo();
    // Both: the bar draws from `plan`, and `task` is what `/task`-style reporting would read.
    app.plan = PlanView::of(&demo_task);
    app.task = Some(demo_task);

    // The workplace button's initial value: in demo mode the bridge won't send
    // `FromAgent::Workplace`, so one is stuffed in locally, letting `--demo` show the tier name
    // on the button and the ticked row in the list.
    app.ws.set_view(WorkplaceView { mode: Mode::Default, root: None });

    // The model chip's initial value, for the same reason: no `FromAgent::Provider` is ever sent in
    // demo mode, and a chip reading "not configured" in every preview screenshot would hide the state
    // that matters — a model with a window, and a list behind it.
    app.mdl.set_view(crate::bridge::ProviderView {
        base_url: "https://api.deepseek.com".into(),
        model: app.model.clone(),
        context_window: u32::try_from(DEFAULT_CONTEXT_WINDOW).unwrap_or(1_000_000),
        max_tokens: 32_000,
        temperature: Some(0.3),
        thinking: aox::config::Thinking::Default,
        ready: true,
        error: None,
    });

    // The "Σ usage" chip **is only drawn when there is a ledger**, and `--demo` has no real
    // calls -- so two records are added, letting the preview show it and the breakdown panel it
    // expands on hover (which is also the only way to verify the panel's position).
    app.usage_log = vec![
        UsageRecord {
            round: 1,
            at: 0,
            calls: 2,
            usage: Usage {
                prompt: Some(9_312),
                completion: Some(1_204),
                total: Some(10_516),
                reasoning: Some(486),
                cached: Some(2_048),
                cache_miss: Some(7_264),
            },
            last_prompt: Some(9_312),
            session_total: Some(10_516),
            session_calls: 2,
        },
        UsageRecord {
            round: 2,
            at: 0,
            calls: 4,
            usage: Usage {
                prompt: Some(24_608),
                completion: Some(2_174),
                total: Some(26_782),
                reasoning: Some(1_130),
                cached: Some(6_144),
                cache_miss: Some(18_464),
            },
            last_prompt: Some(24_608),
            session_total: Some(37_298),
            session_calls: 6,
        },
    ];

    app.items.push(Item::User(aox::i18n::t("Make a plugin that counts files by type in the current directory").into(), Vec::new()));
    app.items.push(Item::Process(Process {
        steps: vec![
            StepView {
                name: "list_plugins".into(),
                plugin: Some("meta".into()),
                purpose: Some(aox::i18n::t("see what the host already has").into()),
                subject: None,
                args: "{}".into(),
                result: Some((false, aox::i18n::t("Plugins loaded: probe_screen, fs").into())),
                diff: None,
                diff_detail: None,
                expanded: false,
                images: Vec::new(),
            },
            StepView {
                name: "create_plugin".into(),
                plugin: Some("meta".into()),
                purpose: Some(aox::i18n::t("make the filestat plugin").into()),
                subject: None,
                args: r#"{"name":"filestat","language":"python"}"#.into(),
                result: Some((false, aox::i18n::t("Plugin filestat created, registered and persisted (1 tool)").into())),
                diff: None,
                diff_detail: None,
                expanded: false,
                images: Vec::new(),
            },
            StepView {
                name: "write_file".into(),
                plugin: None,
                purpose: Some(aox::i18n::t("write the plugin").into()),
                // A file that is **really there**: this row is the demo of "click a tool call's file
                // name and it opens in the file manager", and a path that resolves to nothing would
                // demonstrate the opposite (`plugins/filestat/main.py` only exists if you built the
                // demo plugin). See `open::resolve_relative`.
                subject: Some("plugins/sdk/plugin_sdk.py".into()),
                args: r#"{"path":"plugins/sdk/plugin_sdk.py"}"#.into(),
                result: Some((false, aox::i18n::t("Overwrote plugins/sdk/plugin_sdk.py (2.4 KB)").into())),
                // Demo data for the edit statistic — the row draws it as "+46 / −2", and unfolds
                // into a diff in the very shape `diff_text` produces.
                diff: Some((46, 2)),
                diff_detail: Some(
                    "⋯\n import os\n \n def count(path):\n     out = {}\n-    for f in os.listdir(path):\n+    for f in sorted(os.listdir(path)):\n         ext = os.path.splitext(f)[1]\n         out[ext] = out.get(ext, 0) + 1\n+    return out\n⋯"
                        .into(),
                ),
                expanded: false,
                images: Vec::new(),
            },
            StepView {
                name: "filestat_count".into(),
                plugin: Some("filestat".into()),
                purpose: Some(aox::i18n::t("count the files here").into()),
                subject: None,
                args: r#"{"path":".","ext":null}"#.into(),
                result: Some((false, ".rs 14 · .py 9 · .toml 4 · .md 2 · .lock 1".into())),
                diff: None,
                diff_detail: None,
                expanded: false,
                images: Vec::new(),
            },
        ],
        interim: vec![aox::i18n::t("Check for existing capabilities to reuse first.").into()],
        reasoning: "Counting files by type: the host has no such capability; create a filestat plugin with create_plugin first, then call it."
            .into(),
        elapsed: Some(7.4),
        expanded: false,
        aborted: false,
    }));
    app.items.push(Item::Assistant(
        "Your `filestat` plugin is ready; it provides the tool **filestat_count**.\n\n## Usage\n\n```json\n{ \"path\": \".\", \"ext\": null }\n```\n\nLeave `ext` empty to count all extensions. Result for the current directory:\n\n| Ext | Count |\n|------|------|\n| .rs | 14 |\n| .py | 9 |\n| .toml | 4 |\n| .md | 2 |\n\n> The plugin is persisted under `plugins/filestat/`; it loads automatically on the next start -- no need to rebuild."
            .into(),
    ));
    // ▼▼▼ diagnostics: over-wide content (long code lines / long inline code / long words /
    //      lists / tables) -- to verify that content wider than the measure wraps at the right
    //      edge instead of being clipped.
    //
    // The bare path is **this binary's own path**, not a made-up one: the whole point of that line is
    // that clicking it opens a real Explorer window, and a placeholder that resolves to nothing would
    // only ever show the "path not found" tooltip — which is the opposite of a demo.
    let self_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "E:\\rustapp\\dist\\aox-gui.exe".into());
    app.items.push(Item::Assistant(
        "## Over-wide content\n\n```json\n{\"very_long_key_that_never_wraps\":\"C:/Users/someone/AppData/Local/Temp/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json\",\"n\":1}\n```\n\nThe path `E:/rustapp/plugins/filestat/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.py` is very long.\n\nA bare path in prose, and a URL: {SELF} · https://github.com/forever218/AOX — both are clickable (click the path: Explorer opens with the file selected; click the URL: it opens in your browser).\n\n### Connectivity test\n\n- `httpfetch_raw` → requested bilibili.com, got HTTP 200 and fetched the page text (183539 chars, truncated); network and plugin-process communication are fine.\n\n- `bilibili_video_info` → fetched BV1GJ411x7h7 successfully with full metadata (title \"Never Gonna Give You Up (Official MV)\", publisher \"Sony Music China\", 106M plays).\n\n| Plugin | Language | Tools |\n| --- | --- | --- |\n| bilibili | python | bilibili_video_info (video metadata), bilibili_danmaku (bullet comments), bilibili_comments (comments), bilibili_subtitle (subtitles), bilibili_related (related videos) |\n| httpfetch | python | httpfetch_raw (generic GET for API debugging / web page fetching) |\n\nConclusion: no new or fixed plugins needed -- existing capabilities already cover Bilibili video info fetching + generic web requests, ready to take the job. File I/O, command execution, data export (CSV/Excel) and similar can be added later as needed.\n\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .replace("{SELF}", &self_path),
    ));
    // ▲▲▲ diagnostics
    app.items.push(Item::Note(
        NoteKind::Info,
        aox::i18n::t("Plugin filestat restored (1 tool)").into(),
    ));

    // The activity stream: lay out a typical sequence of events in time order (horizontal
    // timeline: bar width = real elapsed time). Demo mode opens the activity-stream window as
    // well -- it is a big part of the UI already, so a preview should show it.
    {
        let mut a = activity::lock(&app.act);
        a.force_open();
        a.round_begin(aox::i18n::t("Make a plugin that counts files by type in the current directory"), 0.0);
        a.model_begin(0.5);
        a.model_push(aox::i18n::t("Counting files by type: the host has no such capability;"));
        a.model_push(aox::i18n::t("create a filestat plugin with create_plugin first, then call it."));
        a.tool_begin("list_plugins", Some("meta"), "{}", 1.7);
        a.tool_end(false, aox::i18n::t("Plugins loaded: probe_screen, fs"), 2.0);
        a.model_begin(2.1);
        a.model_push(aox::i18n::t("Check for existing capabilities to reuse first."));
        a.tool_begin(
            "create_plugin",
            Some("meta"),
            r#"{"name":"filestat","language":"python"}"#,
            2.8,
        );
        a.tool_note(aox::i18n::t("Timeout limit 120s"));
        a.note(ActKind::Auth, aox::i18n::t("Request authorization · create_plugin"), "{\"name\":\"filestat\"}", 3.6);
        a.note(ActKind::Info, aox::i18n::t("Authorization result"), aox::i18n::t("Allow once"), 5.8);
        a.tool_end(false, aox::i18n::t("Plugin filestat created, registered and persisted (1 tool)"), 6.4);
        a.model_begin(6.5);
        a.tool_begin("filestat_count", Some("filestat"), r#"{"path":".","ext":null}"#, 7.4);
        a.tool_end(false, ".rs 14 · .py 9 · .toml 4 · .md 2 · .lock 1", 8.6);
        a.model_begin(8.7);
        a.model_push(aox::i18n::t("Result in hand; assembling it into a table for the user."));
        a.end_round(11.9);
        a.tick(11.9, aox::i18n::t("Ready"), false);

        // Second turn: the cross-turn, cross-plugin connector effect is demonstrated too.
        //
        // It starts at 11.9, exactly where the first turn ended. That is not a cosmetic choice:
        // the clock stops between turns (`Clock`), so in a real session the gap between one round's
        // last record and the next round's instruction is always zero. Seeding an idle gap here
        // would make the preview show a layout the program can no longer produce.
        a.round_begin(aox::i18n::t("Write the plugin's usage into the README"), 11.9);
        a.model_begin(12.5);
        a.model_push(aox::i18n::t("Just add a \"filestat usage\" section to the README."));
        a.tool_begin("fs_write", Some("fs"), r#"{"path":"README.md"}"#, 13.3);
        a.tool_end(false, aox::i18n::t("Wrote 1.2 KB"), 14.1);
        a.model_begin(14.2);
        a.model_push(aox::i18n::t("Done."));
        a.end_round(16.8);
        a.tick(16.8, aox::i18n::t("Ready"), false);
    }

    // The cache-hit map gets the same treatment: `--demo` must show it too, because "what it looks
    // like while it is working" is most of what a preview is for, and the window is worthless to
    // judge from an empty grid.
    //
    // There is one clock now, so the demo cannot pre-fill an array — it has to **run the front**, and
    // the front only walks while a turn is in flight. So the demo declares itself "in flight" for the
    // length of this block: it is standing in for exactly the one situation that ever fills this map.
    //
    // Two hundred-odd readings, a shade over one lap, so the preview opens on a **full** square
    // instead of one that stops a third of the way round. Each reading holds for one or two tiles, so
    // the map has bands rather than a single dot in the corner.
    //
    // Deterministic rather than random (a fixed pattern, not a generator), so two launches of
    // `--demo` show the same picture and a screenshot can be compared against the previous one.
    //
    // The last tile is written by the call below, so the frame lands on the current reading, and the
    // final numbers are the ones a live window would show: a 31K context of which 24.4K came back
    // from the cache, with the four sections adding up to exactly that context size.
    //
    // Fed through the **real** sequence rather than the shortcuts: sections, then the per-call
    // figures, then the turn's ledger — the same three messages a live turn sends, in the same
    // order. A preview that skipped a step would stop being evidence that the live path works.
    {
        use aox::llm::Usage;
        let mut cv = cache::lock(&app.cache);
        cv.gw.force_open();
        cv.gw.busy = true;
        for k in 0..200u32 {
            let prompt = 6_000 + u64::from(k) * 140;
            // The mix is a *warm* conversation, which is what a long one looks like: most calls land
            // at 93% cached — a hit band with a thin brick strip, the case where the old picture
            // showed nothing at all — a few at 55% (a message appended that the cached prefix could
            // not absorb), and the odd 2% cold start.
            let share = match k % 12 {
                3 | 8 => 0.55,
                5 => 0.02,
                _ => 0.93,
            };
            let cached = (prompt as f64 * share) as u64;
            cv.observe_call(
                &Usage { prompt: Some(prompt), cached: Some(cached), ..Default::default() },
                None,
            );
            // 0.1 s is the largest step `advance` accepts from one frame, and at 10.2 tiles a
            // second it is one tile — so this is "the reading held for 98 ms".
            cv.advance(0.1);
            // Every fifth reading held twice as long: without a few wider bands the strip reads as
            // noise instead of as something that stayed put.
            if k % 5 == 0 {
                cv.advance(0.1);
            }
        }
        cv.observe_context(vec![
            cache::Seg { label: "System prompt", tokens: 5_200 },
            cache::Seg { label: "Tool definitions", tokens: 4_100 },
            cache::Seg { label: "Conversation history", tokens: 20_100 },
            cache::Seg { label: "Your input", tokens: 1_800 },
        ]);
        cv.observe_call(
            &Usage {
                prompt: Some(31_200),
                completion: Some(940),
                total: Some(32_140),
                reasoning: Some(310),
                cached: Some(24_400),
                cache_miss: Some(6_800),
            },
            Some(31_200),
        );
        cv.observe_turn(&UsageRecord {
            round: 3,
            at: 0,
            calls: 2,
            usage: Usage {
                prompt: Some(31_200),
                completion: Some(940),
                total: Some(32_140),
                reasoning: Some(310),
                cached: Some(24_400),
                cache_miss: Some(6_800),
            },
            last_prompt: Some(31_200),
            session_total: Some(74_600),
            session_calls: 7,
        });
        // One step for the live reading, so the frame lands on **it**: the seeded loop ends on a 93%
        // call, and a preview whose newest tile is a hit would show neither the 22%-missed strip nor
        // the partial band the last call really produced.
        cv.advance(0.1);
        // The launch lands with the square all but full and the frame on the live reading. It then
        // **stops**, because nothing is in flight in demo mode — the same rule a real window follows
        // when the turn ends.
        cv.gw.busy = false;
    }

    // The workspace view is seeded too: a file browser previewed on an empty directory would prove
    // nothing about the layout. The root is the **current directory** rather than the app root,
    // because in demo mode the app root is `target/release` — which contains no source to browse —
    // while the current directory is where `--demo` is normally launched from.
    //
    // The two edits are illustrative: the counts and the diff lines are written out here rather
    // than produced by a turn, because in demo mode no turn ever ran. The **paths are real**, so the
    // "File" half of the pane shows actual code from this repository.
    {
        let root = std::env::current_dir().unwrap_or_default();
        wsview::force_open(&app.wsview);
        wsview::set_root(&app.wsview, Some(root.clone()));
        wsview::note_edit(
            &app.wsview,
            root.join("src").join("tool").join("hostfs.rs"),
            Some((4, 2)),
            Some(
                "⋯\n     let existed = path.exists();\n-    let previous = if existed { std::fs::read_to_string(&path).ok() } else { None };\n+    // Read the previous version **before** overwriting it — this is the only moment both\n+    // versions are in hand.\n+    let previous = if existed { std::fs::read_to_string(&path).ok() } else { Some(String::new()) };\n     match std::fs::write(&path, content.as_bytes()) {\n⋯"
                    .to_string(),
            ),
        );
        wsview::note_edit(
            &app.wsview,
            root.join("gui").join("src").join("wsview.rs"),
            Some((6, 0)),
            Some(
                "⋯\n     fn rebuild(&mut self) {\n         self.rows.clear();\n+        let Some(root) = self.root.clone() else {\n+            return;\n+        };\n+        let edited: BTreeSet<String> = self.files.iter().map(|f| key(&f.path)).collect();\n+        walk(&root, 0, &self.expanded, &edited, &mut self.rows);\n     }\n⋯"
                    .to_string(),
            ),
        );
        // Two directories unfolded, so the tree shows its shape instead of a single row.
        wsview::expand(&app.wsview, root.join("src"));
        wsview::expand(&app.wsview, root.join("gui").join("src"));
        // Land on the diff: the window's whole point is "what did this turn change", and an empty
        // right pane would demo the frame without demoing the feature.
        wsview::select(
            &app.wsview,
            root.join("src").join("tool").join("hostfs.rs"),
            wsview::Mode::Diff,
        );
    }
}

