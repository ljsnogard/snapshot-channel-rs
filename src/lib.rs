#![feature(try_trait_v2)]

#![no_std]

mod glimpse_;
mod snapshot_;

pub use snapshot_::{Snapshot, PeekAsync};
pub use glimpse_::{Glimpse, PeekTask, SnapshotError};

pub mod x_deps {
    pub use pincol;

    pub use pincol::x_deps::{abs_sync, atomex, atomic_sync, pin_utils};
}