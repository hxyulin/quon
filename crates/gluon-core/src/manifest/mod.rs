//! Cargo manifest support: reading `Cargo.toml` workspaces and resolving
//! features the way Cargo's resolver 2 does, so Gluon can build crates
//! from their manifests instead of edges duplicated in `gluon.rs`.

pub mod platform;
