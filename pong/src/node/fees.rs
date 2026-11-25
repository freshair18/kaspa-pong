pub fn coins(sompi: u128) -> String {
    format!("{}.{:08}", sompi / 100_000_000, sompi % 100_000_000)
}
