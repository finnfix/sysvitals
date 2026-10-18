//! SysVitals core layer.
//!
//! This crate is the only place where behavior can go wrong: sampling
//! differentials, counter wrap-around, NIC aggregation, unit selection, prefix
//! handling and dual-row layout are all pure logic here so they can be unit
//! tested on any host. The platform display layers are thin
//! glue over these types.
//!
//! Layering contract: no OS-specific windowing or
//! rendering code lives here. `sysinfo` is the only system touchpoint and it
//! is kept behind [`sampler`] so individual metrics can later be swapped for
//! native APIs without changing the public surface.

pub mod config;
pub mod format;
pub mod i18n;
pub mod model;
pub mod sampler;

pub use config::Config;
pub use format::{format_frame, format_percent, format_rate, FormattedRow, Line};
pub use i18n::{t, Language};
pub use model::{Anchor, Metric, Snapshot, Theme};
pub use sampler::{CpuSampler, MemSampler, NetSampler, NetSelection, Sampler};
