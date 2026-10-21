//! The AOX core library.
//!
//! This crate ships two artifacts:
//! - **lib `aox`** (this file): exposes the agent loop, the LLM provider, the plugin host,
//!   the runtimes and the `Ui` abstraction, so other front ends — `gui/` (`aox-gui`) among
//!   them — can reuse them.
//! - **bin `aox`** (`src/main.rs`): the original command-line build. Its behaviour is
//!   identical to what it was before the GUI existed.
//!
//! Both crate roots compile the same set of modules, and the CLI does not reference this
//! lib, so its behaviour is unaffected. Everything that is *not* front-end specific belongs
//! behind the public API here — never copy an implementation into a front end.

pub mod agent;
pub mod agent_loop;
pub mod attach;
pub mod auth;
pub mod compact;
pub mod config;
pub mod history;
pub mod hook;
pub mod i18n;
pub mod image;
pub mod llm;
pub mod message;
pub mod paths;
pub mod plugin;
pub mod prompt;
pub mod recycle;
pub mod runtime;
pub mod runtimes;
pub mod sandbox;
pub mod selftest;
pub mod shell;
pub mod space;
pub mod store;
pub mod sysinfo;
pub mod task;
pub mod tool;
pub mod ui;
pub mod workplace;
