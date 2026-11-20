// forge — GitHub-compatible git forge in Rust (std only, static musl).
// Modules are owned by parallel workers; see ~/workspace/makefun/gcc-rust/SPEC.md.
mod sha1;
mod zlib;
mod pktline;
mod object;
mod pack;
mod store;
mod http;
mod json;
mod githttp;
mod rest;
mod graphql;
mod mcp;
mod app;

fn main() {
    app::run();
}
