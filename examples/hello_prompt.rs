//! Throw-away probe: print the exact system + user message the first-screen greeting sends.
//!
//! The message is built by `hello::generate`, which needs a provider only to send it. This fake one
//! refuses instead of sending, after printing what it was handed — the point is to read the prompt,
//! not to talk to a model.
//!
//!     AOX_LANG=zh cargo run --release --example hello_prompt

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

use aox::hello;
use aox::llm::{ChatReply, LlmProvider, StreamSink};
use aox::message::Message;

struct Echo;

#[async_trait]
impl LlmProvider for Echo {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[Value],
        _sink: Option<&(dyn StreamSink + Send + Sync)>,
    ) -> Result<ChatReply> {
        for m in messages {
            println!("─── [{}] ───", m.role);
            println!("{}", m.text().unwrap_or_default());
            println!();
        }
        Err(anyhow!("probe stops here"))
    }
}

#[tokio::main]
async fn main() {
    println!("lang = {:?}", aox::i18n::lang());
    let _ = hello::generate(&Echo).await;
}
