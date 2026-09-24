//! SCV's black-box tests. They run the `scv` and `scv-server` binaries in
//! temporary instance homes (see [`support::Isolated`]) and talk to them as a
//! client would. Everything is one test binary, so adding a test module costs
//! no extra link step; each module covers one area of the product.

mod config;
mod daemon;
mod delegation;
mod guard;
mod restart;
mod server;
mod support;
mod update;
