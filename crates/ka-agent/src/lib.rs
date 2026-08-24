//! ka engine crate: the turn machine and layered configuration. No I/O
//! beyond the queues; surfaces live elsewhere, wires live in ka-dialect.

mod canned;
pub mod config;
mod engine;
pub mod hands;
mod voice;

pub use config::{Config, ConfigError};
pub use engine::{EngineHandle, StrandChoice, read_waypoint, spawn, spawn_full, spawn_with};
