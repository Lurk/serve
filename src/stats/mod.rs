#![allow(dead_code, unused_imports)] // bin-side scaffold; removed in commit 7 when main wires StatsHandle in

pub mod auth;
pub mod clock;
pub mod config;
pub mod recorder;
pub mod rollup;
pub mod routes;
pub mod store;
pub mod templates;
pub mod writer;

pub use clock::{Clock, SystemClock};
pub use config::{DEFAULT_URL_PREFIX, StatsConfig};
pub use recorder::{RecorderHandle, StatsRecorderLayer};
pub use routes::StatsState;
pub use writer::WriterHandle;
