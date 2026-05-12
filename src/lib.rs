//! kata — multi-project template applier with AI-delegated merge.

pub mod ai;
pub mod applied;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod error;
pub mod git;
pub mod interactive;
pub mod manifest;
pub mod modes;
pub mod paths;
pub mod preset;
pub mod render;
pub mod runner;
pub mod template;
pub mod ui;
pub mod vcs;

pub use error::{Error, Result};
