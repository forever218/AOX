//! The engine behind the `web_fetch` tool (`src/tool/web.rs`).
//! Four parts, deliberately separate from the tool.
//! Egress — *may this host be reached*, and the address classification behind the answer; - charset — what...

pub mod charset;
pub mod egress;
pub mod fetch;
pub mod html;

pub use fetch::{fetch, Fetched, WebError};
