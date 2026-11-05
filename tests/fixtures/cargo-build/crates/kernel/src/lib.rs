#![no_std]

/// Combines the dependencies so every edge is used: a renamed path
/// dependency (`helpers`), a workspace dependency with a feature
/// (`util/extra`), and this package's own `log` feature.
pub fn answer() -> u32 {
    let base = util::base() + util::extra() + helpers::offset();
    if cfg!(feature = "log") { base + 1 } else { base }
}
