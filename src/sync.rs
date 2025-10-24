#![allow(unexpected_cfgs)]

#[cfg(loom)]
pub mod atomic {
    pub use loom::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
}

#[cfg(not(loom))]
pub mod atomic {
    pub use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
}
