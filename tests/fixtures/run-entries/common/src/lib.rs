//! Used by both boards with the same features, so a build of every
//! entry compiles it once. Its greeting comes from `q.env(..)` in
//! quon.rs, so it compiles only when that variable reaches rustc.

pub const GREETING: &str = env!("COMMON_GREETING");
