#![allow(dead_code, unused_imports)] // bin-side scaffold; removed in commit 7 when main wires StatsHandle in

pub mod clock;
pub mod config;

pub use clock::{Clock, SystemClock};
pub use config::{DEFAULT_URL_PREFIX, StatsConfig};
