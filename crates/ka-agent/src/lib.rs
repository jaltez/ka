//! ka engine crate: the turn machine and layered configuration. No I/O
//! beyond the queues; surfaces live elsewhere, wires live in ka-dialect.

pub mod agents;
mod canned;
pub mod checkpoint;
pub mod config;
pub mod conventions;
mod engine;
mod fshooks;
pub mod hands;
pub mod lsp;
pub mod mcp;
pub mod trust;
mod voice;

pub use config::{Config, ConfigError};
pub use engine::{EngineHandle, StrandChoice, read_waypoint, spawn, spawn_full, spawn_with};
