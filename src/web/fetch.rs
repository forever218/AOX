//! One request, from URL to bytes.
//! # What is done by hand, and why.
//! `reqwest`'s own redirect policy cannot do three things this tool needs: re-check `web egress` on every hop...

use std::path::PathBuf;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderName, HeaderValue, CONTENT_TYPE, LOCATION};
use reqwest::{Client, Method};
use url::Url;

use super::charset::{self, Detected};
use super::egress::{self, Policy};

/// A name + a readable fix. Everything the tool reports about a failure goes through this, so "the
/// error names the key that changes the outcome" is structural rather than a habit.
#[derive(Debug, Clone)]
pub struct WebError {
    pub message: String,
    pub hint: Option<String>,
}

impl WebError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Two lines at most: what happened, then what to change
    pub fn render(&self) -> String {
        match &self.hint {
            Some(h) => format!("{}\nhint: {}", self.message, h),
            None => self.message.clone(),
        }
    }
}

/// The `User-Agent` sent when `[web] user_agent` is empty
pub const DEFAULT_UA: &str = concat!("AOX/", env!("CARGO_PKG_VERSION"), " (local agent)");

/// Everything one call tells the engine. The tool fills this in; nothing here reads config itself.
pub struct Request {
    pub url: Url,
    pub method: Method,
    pub body: Option<String>,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub timeout: Duration,
    pub max_bytes: usize,
    pub user_agent: String,
    pub ca_bundle: Option<PathBuf>,
    pub insecure_tls: bool,
    pub max_redirects: u32,
    pub policy: Policy,
    pub hosts: Vec<String>,
    pub charset_override: Option<String>,
    pub charset_fallback: String,
}

/// What came back. Non-2xx statuses are **not** errors here — the tool decides how to phrase them,
/// because the body of a 403 often explains more than the status does.
pub struct Fetched {
    pub final_url: Url,
    pub status: u16,
    /// The raw header value (`text/html; charset=gbk`), empty when absent.
    pub content_type: String,
    pub charset: Detected,
    pub body: Vec<u8>,
    /// The body hit `max_bytes`; what is in `body` is a prefix
    pub truncated: bool,
    pub elapsed: Duration,
    /// `(status, url)` for each hop actually followed, in order.
    pub hops: Vec<(u16, String)>,
}

pub async fn fetch(req: Request) -> Result<Fetched, WebError> {
    let started = Instant::now();
    let client = build_client(&req)?;

    let mut url = req.url.clone();
    let mut method = req.method.clone();
    let mut body = req.body.clone();
    let mut headers = req.headers.clone();
    let mut hops: Vec<(u16, String)> = Vec::new();
    let mut retried = false;

    loop {
        egress::check(&url, req.policy, &req.hosts).await?;

        let mut rb = client.request(method.clone(), url.clone());
        if let Some(b) = &body {
            rb = rb.body(b.clone());
        }
        for (name, value) in &headers {
            rb = rb.header(name.clone(), value.clone());
        }
        let resp = match rb.send().await {
            Ok(resp) => resp,
            // Exactly one retry, and only for a GET that never got a response. A timeout is not
            // retried: waiting twice as long is not an improvement.
            Err(e) if !retried && method == Method::GET && e.is_connect() => {
                retried = true;
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
            Err(e) => return Err(classify(&e, req.timeout)),
        };

        let status = resp.status();

        if status.is_redirection() {
            let location =
                resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
            drop(resp);
            let Some(location) = location else {
                return Err(WebError::new(crate::tf!(
                    "HTTP {} redirect without a Location header",
                    status.as_u16()
                ))
                .hint("call the final URL directly"));
            };
            let next = url.join(&location).map_err(|e| {
                WebError::new(crate::tf!("bad redirect target \"{}\": {}", location, e))
            })?;
            if !matches!(next.scheme(), "http" | "https") {
                return Err(WebError::new(crate::tf!(
                    "refused a redirect to the {} scheme",
                    next.scheme()
                ))
                .hint("only http and https are fetched"));
            }
            if url.scheme() == "https" && next.scheme() == "http" && req.policy != Policy::Any {
                return Err(WebError::new(crate::tf!(
                    "refused an https → http redirect ({})",
                    next
                ))
                .hint("the page downgrades to plaintext; set [web] egress = \"any\" if that is intended"));
            }
            if hops.len() as u32 >= req.max_redirects {
                return Err(WebError::new(crate::tf!(
                    "more than {} redirect(s)",
                    req.max_redirects
                ))
                .hint("raise [web] max_redirects, or call the final URL directly"));
            }
            hops.push((status.as_u16(), next.to_string()));
            // A redirect to another host must not carry what was meant for the first one.
            if next.host_str() != url.host_str() {
                headers.clear();
            }
            // 301/302/303 rewrite to GET; 307/308 keep the method *and* the body.
            if matches!(status.as_u16(), 301 | 302 | 303) {
                method = Method::GET;
                body = None;
            }
            url = next;
            continue;
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = resp.chunk().await.map_err(|e| classify(&e, req.timeout))? {
            if buf.len() + chunk.len() > req.max_bytes {
                let room = req.max_bytes.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            buf.extend_from_slice(&chunk);
        }

        let detected = charset::detect(
            &buf,
            (!content_type.is_empty()).then_some(content_type.as_str()),
            req.charset_override.as_deref(),
            &req.charset_fallback,
        )?;

        return Ok(Fetched {
            final_url: url,
            status: status.as_u16(),
            content_type,
            charset: detected,
            body: buf,
            truncated,
            elapsed: started.elapsed(),
            hops,
        });
    }
}

fn build_client(req: &Request) -> Result<Client, WebError> {
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(req.timeout)
        .connect_timeout(Duration::from_secs(10).min(req.timeout))
        .user_agent(if req.user_agent.trim().is_empty() { DEFAULT_UA } else { req.user_agent.trim() })
        .danger_accept_invalid_certs(req.insecure_tls);

    // A local target must not be sent to the ambient proxy: a request to `127.0.0.1` would be delivered...
    if egress::is_local_literal(&req.url) {
        builder = builder.no_proxy();
    }

    if let Some(path) = &req.ca_bundle {
        let pem = std::fs::read(path).map_err(|e| {
            WebError::new(crate::tf!("cannot read [web] ca_bundle {}: {}", path.display(), e))
                .hint("point ca_bundle at a PEM file, or clear it to use the bundled roots")
        })?;
        let blocks = pem_blocks(&pem);
        if blocks.is_empty() {
            return Err(WebError::new(crate::tf!(
                "[web] ca_bundle {} contains no CERTIFICATE block",
                path.display()
            ))
            .hint("export the root as PEM (-----BEGIN CERTIFICATE-----), not DER/P7B"));
        }
        for block in blocks {
            let cert = reqwest::Certificate::from_pem(&block).map_err(|e| {
                WebError::new(crate::tf!("bad certificate in [web] ca_bundle: {}", e))
            })?;
            builder = builder.add_root_certificate(cert);
        }
    }

    builder.build().map_err(|e| {
        WebError::new(crate::tf!("cannot build the HTTP client: {}", e))
            .hint("this is a host problem, not the URL; check [web] ca_bundle / insecure_tls")
    })
}

/// Split a PEM bundle into individual certificates. `Certificate::from_pem` takes one per call.
fn pem_blocks(bytes: &[u8]) -> Vec<Vec<u8>> {
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    const END: &[u8] = b"-----END CERTIFICATE-----";
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(start) = find(&bytes[at..], BEGIN) {
        let start = at + start;
        let Some(end) = find(&bytes[start..], END) else { break };
        let end = start + end + END.len();
        out.push(bytes[start..end].to_vec());
        at = end;
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Turn a `reqwest` error into two lines a model can act on.
fn classify(e: &reqwest::Error, timeout: Duration) -> WebError {
    let raw = e.to_string();
    if e.is_timeout() {
        return WebError::new(crate::tf!("timed out after {} s", timeout.as_secs())).hint(
            "raise the call's timeout_secs (ceiling: [web] max_timeout_secs); for a large file, \
             save=true and a longer deadline",
        );
    }
    if e.is_connect() {
        let lower = raw.to_ascii_lowercase();
        let tls = lower.contains("certificate")
            || lower.contains("tls")
            || lower.contains("handshake")
            || lower.contains("unknown issuer");
        return if tls {
            WebError::new(crate::tf!("TLS handshake failed: {}", raw)).hint(
                "rustls uses the bundled roots, not the Windows store. Point [web] ca_bundle at the \
                 PEM, or set insecure_tls = true if you accept the risk",
            )
        } else {
            WebError::new(crate::tf!("cannot connect: {}", raw))
                .hint("check the network; behind a proxy set HTTPS_PROXY / HTTP_PROXY")
        };
    }
    if e.is_redirect() {
        return WebError::new(crate::tf!("redirect failed: {}", raw))
            .hint("raise [web] max_redirects, or call the final URL directly");
    }
    if e.is_builder() {
        return WebError::new(crate::tf!("malformed request: {}", raw))
            .hint("check the headers: names must be tokens, values must not contain CR/LF");
    }
    WebError::new(crate::tf!("request failed: {}", raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pem_bundle_is_split_into_its_certificates() {
        let pem = b"junk\n-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n\
                    between\n-----BEGIN CERTIFICATE-----\nBBB\n-----END CERTIFICATE-----\ntail";
        let blocks = pem_blocks(pem);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with(b"-----BEGIN"));
        assert!(blocks[1].ends_with(b"-----END CERTIFICATE-----"));
    }

    #[test]
    fn a_file_without_certificates_yields_nothing() {
        assert!(pem_blocks(b"not a pem").is_empty());
    }

    #[test]
    fn the_error_render_is_at_most_two_lines() {
        let e = WebError::new("boom").hint("fix it");
        assert_eq!(e.render(), "boom\nhint: fix it");
        assert_eq!(WebError::new("boom").render(), "boom");
    }
}
