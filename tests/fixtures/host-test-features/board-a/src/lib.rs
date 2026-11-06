#[cfg(test)]
mod tests {
    #[test]
    fn uses_chip_a() {
        assert_eq!(chip::NAME, "a");
    }
}
