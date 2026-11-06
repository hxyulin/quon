#[cfg(all(feature = "a", feature = "b"))]
compile_error!("enable one chip feature, not both");

#[cfg(feature = "a")]
pub const NAME: &str = "chip a";
#[cfg(feature = "b")]
pub const NAME: &str = "chip b";
#[cfg(not(any(feature = "a", feature = "b")))]
pub const NAME: &str = "no chip";

#[cfg(test)]
mod tests {
    #[test]
    fn has_a_name() {
        assert!(!super::NAME.is_empty());
    }
}
