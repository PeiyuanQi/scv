//! SCV's black-box tests. They run the `scv` and `scv-server` binaries in
//! temporary instance homes (see [`support::Isolated`]) and talk to them as a
//! client would. Everything is one test binary, so adding a test module costs
//! no extra link step; each module covers one area of the product.

#![allow(
    clippy::unwrap_used,
    reason = "a test fails loudly on the first unexpected error"
)]

mod config;
mod confirm;
mod daemon;
mod delegation;
mod feature_flow;
mod guard;
mod restart;
mod server;
mod support;
mod update;
