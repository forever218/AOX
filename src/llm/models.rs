//! Asking an endpoint which models it serves — the "scan" behind the Model tab's model list.
//! There is no standard here, only a convention and a pile of near-misses, so this module is written...
//! * Where the list lives is decided by crate::config::ProviderConfig::models_urls (a versioned base gets...

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;

/// One model as the endpoint described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    /// Only set when the response carried one (`context_length`, `max_model_len`, …).
    pub window: Option<u32>,
}

/// The outcome of a successful scan.
#[derive(Debug, Clone)]
pub struct Scan {
    /// The URL that answered — worth keeping so the next scan asks once instead of twice.
    pub url: String,
    pub models: Vec<ModelEntry>,
}

/// Hard ceiling for one probe. Long enough for a slow gateway, short enough that the panel does not
/// feel hung.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(8);

/// Keys different servers use for "how big is the window". Checked in this order.
const WINDOW_KEYS: [&str; 6] = [
    "context_length",
    "max_model_len",
    "context_window",
    "max_context_length",
    "input_token_limit",
    "max_input_tokens",
];

/// List the models served by `base_url`.
pub async fn list(base_url: &str, key: Option<&str>) -> Result<Scan> {
    let cfg = crate::config::ProviderConfig {
        base_url: base_url.trim().to_string(),
        model: String::new(),
        api_key: key.map(str::to_string),
        api_key_env: None,
        max_tokens: None,
        temperature: None,
        stream: true,
        context_window: None,
        thinking: Default::default(),
        thinking_wire: Default::default(),
    };
    let urls = cfg.models_urls();
    let client = super::endpoint_client(base_url).timeout(SCAN_TIMEOUT).build()?;
    let mut last: Option<String> = None;

    for url in &urls {
        let mut req = client.get(url).header("accept", "application/json");
        if let Some(k) = key.map(str::trim).filter(|k| !k.is_empty()) {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                // No HTTP status at all: DNS, refused connection, TLS, timeout.
                return Err(anyhow!(crate::tf!(
                    "cannot reach {}: {}",
                    url,
                    format!("{e}")
                )));
            }
        };
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(anyhow!(crate::tf!(
                "the endpoint rejected the key (HTTP {}). Check that the key is complete and belongs to this endpoint.",
                status.as_u16()
            )));
        }
        if status.as_u16() == 404 {
            last = Some(format!("{url} → 404"));
            continue; // the other candidate shape may well work
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(crate::tf!(
                "{} answered HTTP {}: {}",
                url,
                status.as_u16(),
                snippet(&body)
            )));
        }
        let body = resp.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&body).map_err(|_| {
            anyhow!(crate::tf!(
                "{} did not return JSON — it is probably not an OpenAI-compatible endpoint: {}",
                url,
                snippet(&body)
            ))
        })?;
        let models = parse(&json);
        if models.is_empty() {
            return Err(anyhow!(crate::tf!(
                "{} answered, but the list of models is empty. The endpoint may need a different path, or the key may only see part of the catalogue.",
                url
            )));
        }
        return Ok(Scan { url: url.clone(), models });
    }

    Err(anyhow!(crate::tf!(
        "no model list here (tried {}). Fill the model name in by hand — this endpoint may not support listing.",
        last.unwrap_or_else(|| urls.join(", "))
    )))
}

/// Send one tiny chat request, to answer "does this endpoint + key + model actually work".
pub async fn probe(base_url: &str, key: Option<&str>, model: &str) -> Result<String> {
    let cfg = crate::config::ProviderConfig {
        base_url: base_url.trim().to_string(),
        model: model.trim().to_string(),
        api_key: key.map(str::to_string),
        api_key_env: None,
        max_tokens: None,
        temperature: None,
        stream: false,
        context_window: None,
        thinking: Default::default(),
        thinking_wire: Default::default(),
    };
    if cfg.base_url.is_empty() {
        return Err(anyhow!(crate::i18n::t("fill in the endpoint first")));
    }
    if cfg.model.is_empty() {
        return Err(anyhow!(crate::i18n::t("fill in the model name first")));
    }
    let url = cfg.chat_url();
    let client = super::endpoint_client(&cfg.base_url).timeout(PROBE_TIMEOUT).build()?;
    let mut req = client.post(&url).header("accept", "application/json").json(&serde_json::json!({
        "model": cfg.model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
        "stream": false,
    }));
    if let Some(k) = key.map(str::trim).filter(|k| !k.is_empty()) {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let started = std::time::Instant::now();
    let resp = req.send().await.map_err(|e| {
        // No status at all: DNS, refused connection, TLS, timeout. The address is included because a
        // typo in the host is the most common cause and "cannot connect" alone does not show it.
        anyhow!(crate::tf!("cannot reach {}: {}", url, format!("{e}")))
    })?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let ms = started.elapsed().as_millis();
    match status {
        200..=299 => Ok(crate::tf!("HTTP {} · replied in {} ms", status, ms)),
        401 | 403 => Err(anyhow!(crate::tf!(
            "the endpoint rejected the key (HTTP {})",
            status
        ))),
        404 => Err(anyhow!(crate::tf!(
            "{} answered 404 — the endpoint address is probably missing or has an extra path segment",
            url
        ))),
        400 => Err(anyhow!(crate::tf!(
            "the endpoint rejected the request (HTTP 400): {}. The model name is the usual suspect.",
            snippet(&body)
        ))),
        _ => Err(anyhow!(crate::tf!("HTTP {}: {}", status, snippet(&body)))),
    }
}

/// Ceiling for the "test connection" probe. Shorter than [`SCAN_TIMEOUT`]: this one waits for a model
/// to *generate* something, and a person watching a button has less patience than for a list.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Parse the three shapes a "model list" comes in.
pub fn parse(v: &Value) -> Vec<ModelEntry> {
    let arr = v
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| v.get("models").and_then(Value::as_array))
        .or_else(|| v.as_array());
    let Some(arr) = arr else {
        return Vec::new();
    };

    let mut out: Vec<ModelEntry> = Vec::new();
    for it in arr {
        let id = match it {
            // A bare string in an array (`["m1","m2"]`) — some proxies do exactly this.
            Value::String(s) => s.trim().to_string(),
            Value::Object(o) => o
                .get("id")
                .or_else(|| o.get("name"))
                .or_else(|| o.get("model"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
            _ => continue,
        };
        if id.is_empty() || out.iter().any(|e| e.id == id) {
            continue;
        }
        let window = it.as_object().and_then(|o| {
            WINDOW_KEYS
                .iter()
                .find_map(|k| o.get(*k).and_then(as_u32))
        });
        out.push(ModelEntry { id, window });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// A window may arrive as a number, a float, or a string — all three are seen in the wild.
fn as_u32(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().parse::<u64>().ok().and_then(|n| u32::try_from(n).ok()),
        _ => None,
    }
}

/// First 160 characters of a response body, on one line — enough to recognise the error, short enough
/// to sit in a panel's status line.
fn snippet(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = one.chars().take(160).collect();
    if one.chars().count() > 160 {
        out.push('…');
    }
    if out.is_empty() {
        out.push_str(crate::i18n::t("(empty response)"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The OpenAI shape, including the window: OpenRouter returns `context_length`, and that is the
    /// value the Model tab should prefer over any built-in table.
    #[test]
    fn the_openai_shape_is_read_with_its_window() {
        let v = json!({"object":"list","data":[
            {"id":"deepseek-flash","object":"model","context_length":1000000},
            {"id":"some-model"}
        ]});
        let m = parse(&v);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "deepseek-flash");
        assert_eq!(m[0].window, Some(1_000_000));
        assert_eq!(m[1].window, None, "no field reported means no window, not a guess");
    }

    /// Ollama's `/v1/models` answers `{"models":[{"name":…,"model":…}]}`; the native endpoint uses
    /// `name`. Either has to work, or scanning a local install looks broken.
    #[test]
    fn the_ollama_shape_uses_name() {
        let v = json!({"models":[{"name":"qwen3:8b","model":"qwen3:8b"}]});
        assert_eq!(parse(&v)[0].id, "qwen3:8b");
    }

    /// A bare array, and bare strings inside it — proxies that re-wrap someone else's list do this.
    #[test]
    fn a_bare_array_of_strings_is_accepted() {
        let v = json!(["m-a", "m-b"]);
        let m = parse(&v);
        assert_eq!(m.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["m-a", "m-b"]);
    }

    /// Duplicates collapse and the order is stable: the GUI keeps this list across frames, and a
    /// reshuffling list would make rows jump under the pointer.
    #[test]
    fn duplicates_collapse_and_the_order_is_stable() {
        let v = json!({"data":[{"id":"b"},{"id":"a"},{"id":"b"}]});
        let m = parse(&v);
        assert_eq!(m.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    /// Garbage in, empty list out — the caller turns that into "this endpoint cannot list", never into
    /// a panic or a half-read list.
    #[test]
    fn a_response_without_a_list_yields_nothing() {
        assert!(parse(&json!({"error":"nope"})).is_empty());
        assert!(parse(&json!("just a string")).is_empty());
        assert!(parse(&json!({"data":[{"no_id":1}, 42]})).is_empty());
    }

    /// The window may be a string (`"128000"`) on some servers.
    #[test]
    fn a_window_sent_as_a_string_still_counts() {
        let v = json!({"data":[{"id":"m","max_model_len":"32768"}]});
        assert_eq!(parse(&v)[0].window, Some(32_768));
    }

    // ── end to end, against a server in this process ──.

    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve a fixed little OpenAI-compatible surface on an ephemeral port; return the port.
    fn stub() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = req.lines().next().unwrap_or("").to_string();
                let ok = req.to_ascii_lowercase().contains("authorization: bearer sk-stub");
                let (code, reason, body) = if line.contains("/v1/models") {
                    (404, "Not Found", String::new())
                } else if line.contains("/models") {
                    if ok {
                        (
                            200,
                            "OK",
                            r#"{"object":"list","data":[{"id":"stub-large","context_length":1000000},{"id":"stub-small"}]}"#
                                .to_string(),
                        )
                    } else {
                        (401, "Unauthorized", "no key".to_string())
                    }
                } else {
                    (404, "Not Found", String::new())
                };
                let resp = format!(
                    "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        port
    }

    /// The whole path a "Scan" press takes: the first candidate 404s, the second answers, and the window that came...
    #[tokio::test]
    async fn a_scan_falls_through_to_the_second_candidate_and_keeps_the_window() {
        let port = stub();
        let scan = list(&format!("http://127.0.0.1:{port}"), Some("sk-stub"))
            .await
            .expect("the second candidate answers");
        assert!(scan.url.ends_with("/models"), "answered url: {}", scan.url);
        assert!(!scan.url.contains("/v1/"), "the fallback url is the bare one: {}", scan.url);
        assert_eq!(scan.models.len(), 2);
        let large = scan.models.iter().find(|m| m.id == "stub-large").expect("listed");
        assert_eq!(large.window, Some(1_000_000));
        assert_eq!(scan.models[1].window, None, "a model that reported nothing gets no guess");
    }

    /// A rejected key must be reported **as a rejected key**. Falling through to "no model list here"
    /// would send the user looking at endpoint paths when the problem is the key.
    #[tokio::test]
    async fn a_rejected_key_is_named_as_such() {
        let port = stub();
        let err = list(&format!("http://127.0.0.1:{port}"), Some("wrong"))
            .await
            .expect_err("401 has to fail the scan");
        let text = format!("{err:#}");
        assert!(text.contains("rejected the key"), "got: {text}");
    }

    /// Nothing listening: the message has to carry the address that was tried, because a typo in the
    /// host is the most common cause and "cannot reach the endpoint" alone does not show it.
    #[tokio::test]
    async fn an_unreachable_endpoint_names_the_address() {
        // Bind and immediately drop, to get a port that was free a moment ago.
        let port = {
            let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
            l.local_addr().expect("addr").port()
        };
        let err = list(&format!("http://127.0.0.1:{port}"), None)
            .await
            .expect_err("nothing is listening there");
        let text = format!("{err:#}");
        assert!(text.contains(&port.to_string()), "got: {text}");
    }

    /// "Test connection" has to tell the three interesting failures apart, because they send the user
    /// to three different places: a rejected key, a wrong path, a wrong model name.
    #[tokio::test]
    async fn the_probe_names_the_reason_it_failed() {
        let port = stub_chat();
        let base = format!("http://127.0.0.1:{port}");

        let ok = probe(&base, Some("sk-stub"), "good-model").await.expect("answers 200");
        assert!(ok.contains("HTTP 200"), "got: {ok}");

        let err = format!("{:#}", probe(&base, Some("wrong"), "good-model").await.unwrap_err());
        assert!(err.contains("rejected the key"), "got: {err}");

        let err = format!("{:#}", probe(&base, Some("sk-stub"), "nope").await.unwrap_err());
        assert!(err.contains("400"), "a bad model name is a 400: {err}");
        assert!(err.contains("nope"), "and the body is quoted: {err}");
    }

    /// A POST-only surface that behaves like a real chat route: 200 for a known model, 401 without the
    /// key, 400 for anything else.
    fn stub_chat() -> u16 {
        use std::io::{Read, Write};
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let ok_key = req.to_ascii_lowercase().contains("bearer sk-stub");
                let ok_model = req.contains("\"good-model\"");
                let (code, reason, body) = match (ok_key, ok_model) {
                    (false, _) => (401, "Unauthorized", r#"{"error":"bad key"}"#.to_string()),
                    (true, false) => (
                        400,
                        "Bad Request",
                        r#"{"error":{"message":"model not found: nope"}}"#.to_string(),
                    ),
                    (true, true) => (
                        200,
                        "OK",
                        r#"{"choices":[{"message":{"role":"assistant","content":"p"},"finish_reason":"length"}]}"#
                            .to_string(),
                    ),
                };
                let resp = format!(
                    "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        port
    }

    /// A loopback endpoint must bypass the proxy: this project's own sandbox sets `HTTP_PROXY`, and a request...
    #[test]
    fn a_local_endpoint_is_not_sent_through_a_proxy() {
        assert!(super::super::is_loopback("http://127.0.0.1:11434"));
        assert!(super::super::is_loopback("http://localhost:1234/v1"));
        assert!(super::super::is_loopback("https://[::1]:8080/v1"));
        assert!(!super::super::is_loopback("https://api.deepseek.com"));
        assert!(!super::super::is_loopback("http://192.168.1.9:11434"));
    }

    /// `models_urls` and this parser have to agree on which base goes where; this is the pair that the
    /// GUI's "remember the URL that answered" optimisation rests on.
    #[test]
    fn a_versioned_base_gets_one_candidate_and_a_bare_host_gets_two() {
        use crate::config::ProviderConfig;
        let mk = |b: &str| ProviderConfig {
            base_url: b.into(),
            model: "m".into(),
            api_key: None,
            api_key_env: None,
            max_tokens: None,
            temperature: None,
            stream: true,
            context_window: None,
            thinking: Default::default(),
            thinking_wire: Default::default(),
        };
        assert_eq!(mk("https://x.dev/v1").models_urls(), ["https://x.dev/v1/models"]);
        assert_eq!(
            mk("https://api.deepseek.com").models_urls(),
            ["https://api.deepseek.com/v1/models", "https://api.deepseek.com/models"]
        );
        // A full chat URL is the third way people write `base_url`, and it must not become
        // `…/chat/completions/v1/models`.
        assert_eq!(
            mk("https://x.dev/v1/chat/completions").models_urls(),
            ["https://x.dev/v1/models"]
        );
    }
}
