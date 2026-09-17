//! ka engine crate: the turn machine and layered configuration. No I/O
//! beyond the queues; surfaces live elsewhere, wires live in ka-dialect.

pub mod agents;
mod canned;
pub mod checkpoint;
pub mod config;
pub mod conventions;
pub mod dap;
mod engine;
mod fshooks;
pub mod hands;
pub mod lsp;
pub mod mcp;
pub mod trust;
pub mod voice;
pub mod wire;

pub use config::{Config, ConfigError};
pub use engine::{
    EngineHandle, StrandChoice, effective_debug_cfg, effective_lsp_cfg, lsp_hands, read_waypoint,
    spawn, spawn_full, spawn_with, spawn_with_speaker,
};
