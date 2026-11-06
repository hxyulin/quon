#[cfg(all(feature = "a", feature = "b"))]
compile_error!("features `a` and `b` select different chips");

#[cfg(feature = "a")]
pub const NAME: &str = "a";
#[cfg(feature = "b")]
pub const NAME: &str = "b";
