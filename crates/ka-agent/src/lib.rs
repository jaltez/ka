//! ka engine crate: the turn machine and layered configuration. No I/O
//! beyond the queues; surfaces and wires live elsewhere.

mod canned;
pub mod config;
mod engine;

pub use config::{Config, ConfigError};
pub use engine::{EngineHandle, spawn};
