//! Discord snowflakes: ids that sort by creation time.
//!
//! A snowflake is 64 bits, of which the top 42 are the milliseconds since the first second of
//! 2015 (the Discord epoch) when the thing was created. Message ids in one channel therefore
//! compare in the order the messages were sent, and an id built from a time can stand in for "the
//! first message after this moment" in a pagination parameter
//! (<https://discord.com/developers/docs/reference#snowflake-ids-in-pagination>).

/// 2015-01-01T00:00:00Z in milliseconds since the Unix epoch.
pub const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// How far the creation time is shifted left in a snowflake.
const TIMESTAMP_SHIFT: u32 = 22;

/// The largest number of milliseconds since the Discord epoch that fits the 42 bits.
const MAX_TIMESTAMP: u64 = (1 << 42) - 1;

/// When the thing with this snowflake was created, in milliseconds since the Unix epoch.
pub fn unix_ms(id: u64) -> u64 {
    (id >> TIMESTAMP_SHIFT) + DISCORD_EPOCH_MS
}

/// The smallest snowflake created at `unix_ms`: every id created at or after that millisecond is
/// at least this. A time before the Discord epoch gives 0, which Discord also accepts as the
/// start of time.
pub fn at_unix_ms(unix_ms: u64) -> u64 {
    unix_ms.saturating_sub(DISCORD_EPOCH_MS).min(MAX_TIMESTAMP) << TIMESTAMP_SHIFT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snowflake from Discord's documentation: 175928847299117063 was created on 2016-04-30
    /// at 11:18:25.796 UTC.
    #[test]
    fn it_reads_the_creation_time_of_the_documented_example() {
        // 2016-04-30T11:18:25.796Z
        assert_eq!(unix_ms(175_928_847_299_117_063), 1_462_015_105_796);
    }

    #[test]
    fn an_id_built_from_a_time_is_the_first_of_that_millisecond() {
        let ms = 1_462_015_105_796;
        let id = at_unix_ms(ms);
        assert_eq!(unix_ms(id), ms);
        assert_eq!(
            id & ((1 << 22) - 1),
            0,
            "worker, process, and increment are zero"
        );
        // The documented example was created in that millisecond, so it is not before it.
        assert!(175_928_847_299_117_063 >= id);
        // A message of the next millisecond is after every id of this one.
        assert!(at_unix_ms(ms + 1) > id | ((1 << 22) - 1));
    }

    #[test]
    fn times_outside_the_range_clamp() {
        assert_eq!(at_unix_ms(0), 0);
        assert_eq!(at_unix_ms(DISCORD_EPOCH_MS), 0);
        assert_eq!(at_unix_ms(DISCORD_EPOCH_MS + 1), 1 << 22);
        assert_eq!(at_unix_ms(u64::MAX), MAX_TIMESTAMP << 22);
    }
}
