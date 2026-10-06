//! Timestamp conversion helpers.

const FILETIME_UNIX_EPOCH_MS: i64 = 11_644_473_600_000;

/// Windows FILETIME (100 ns ticks since 1601) to unix epoch milliseconds. `0` stays `0`.
pub fn filetime_to_unix_ms(ft: u64) -> i64 {
    if ft == 0 {
        return 0;
    }
    (ft / 10_000) as i64 - FILETIME_UNIX_EPOCH_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_value() {
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), 0);
        assert_eq!(
            filetime_to_unix_ms(133_000_000_000_000_000),
            1_655_526_400_000
        );
        assert_eq!(filetime_to_unix_ms(0), 0);
    }
}
