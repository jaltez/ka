//! ka engine crate: the turn machine and layered configuration. No I/O
//! beyond the queues; surfaces live elsewhere, wires live in ka-dialect.

pub mod agents;
mod canned;
pub mod config;
pub mod conventions;
mod engine;
pub mod hands;
pub mod mcp;
mod voice;

pub use config::{Config, ConfigError};
pub use engine::{EngineHandle, StrandChoice, read_waypoint, spawn, spawn_full, spawn_with};
