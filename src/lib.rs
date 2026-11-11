//! The AOX core library.
//! This crate ships two artifacts: - lib `aox` (this file): exposes the agent loop, the LLM provider, the plugin...
//! Both crate roots compile the same set of modules, and the CLI does not reference this lib, so its behaviour...

pub mod agent;
pub mod agent_loop;
pub mod attach;
pub mod auth;
pub mod bootstrap;
pub mod checks;
pub mod compact;
pub mod config;
pub mod hello;
pub mod history;
pub mod hook;
pub mod i18n;
pub mod image;
pub mod jobs;
pub mod llm;
pub mod message;
pub mod modelbook;
pub mod paths;
pub mod notes;
pub mod plugin;
pub mod prompt;
pub mod provider_live;
pub mod recovery;
pub mod recycle;
/// Host side of AOX Remote (`aox remote`): pairing, the socket, and the `Ui` that speaks it.
pub mod remote;
pub mod repeat;
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
pub mod web;
pub mod workplace;
