#[cfg(test)]
mod tests {
    #[test]
    fn uses_chip_b() {
        assert_eq!(chip::NAME, "b");
    }
}
