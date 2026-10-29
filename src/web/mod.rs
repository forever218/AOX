//! The engine behind the `web_fetch` tool (`src/tool/web.rs`).
//!
//! Four parts, deliberately separate from the tool:
//!
//! - [`egress`] — *may this host be reached*, and the address classification behind the answer;
//! - [`charset`] — what the bytes say they are, then decoding them;
//! - [`fetch`] — one request: redirects followed by hand, a cap counted after decompression, a
//!   total deadline, and error text that names the config key that fixes it;
//! - [`html`] — bytes to Markdown: the tokenizer, the readability-style scorer, the chrome that
//!   gets dropped.
//!
//! The tool owns what the model *sees* (the envelope, the budget, the path a saved file lands on);
//! this module owns everything that touches the network. That split is the same one
//! `src/tool/shell.rs` uses for `src/shell/`: the tool is the contract, the module is the machinery.

pub mod charset;
pub mod egress;
pub mod fetch;
pub mod html;

pub use fetch::{fetch, Fetched, WebError};
