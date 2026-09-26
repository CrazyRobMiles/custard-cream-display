//! custard-cream-display: a touch-screen picture frame that shows pictures
//! from a custard cream server.

#![no_std]

extern crate alloc;

pub mod caption;
pub mod config;
pub mod fetcher;
pub mod frame;
pub mod heap;
pub mod http;
pub mod hw;
pub mod network;
pub mod picture;
pub mod platform;
pub mod server;
pub mod state;

pub mod ui {
    slint::include_modules!();
}
