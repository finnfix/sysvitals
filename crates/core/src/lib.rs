//! SysVitals core layer.
//!
//! This crate is the only place where behavior can go wrong: sampling
//! differentials, counter wrap-around, NIC aggregation, unit selection, prefix
//! handling, dual-row layout and history persistence are all logic here so
//! they can be unit tested on any host. The platform display layers are thin
//! glue over these types.
//!
//! Layering contract: no OS-specific windowing or rendering code lives here.
//! `sysinfo` (sampling) and `rusqlite` (history) are the only system
//! touchpoints; each is kept behind its own module ([`sampler`], [`store`])
//! so either can be swapped without changing the public surface.

pub mod backdrop;
pub mod color;
pub mod config;
pub mod format;
pub mod i18n;
pub mod logger;
pub mod model;
pub mod sampler;
pub mod store;

pub use config::Config;
pub use format::{format_frame, format_frame_vertical, FormattedRow, Line};
pub use i18n::{t, Language};
pub use model::{Anchor, Metric, Snapshot, Theme};
pub use sampler::{NetSelection, Sampler};
pub use store::{floor_to_grid, spread_label_indices, Bucket, Store};
