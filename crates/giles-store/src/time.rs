//! Timestamp columns: Unix milliseconds in an `INTEGER`.

use giles_types::Timestamp;

use crate::{Error, Result};

/// A timestamp as stored: Unix milliseconds. Sub-millisecond precision is dropped.
pub(crate) fn to_millis(at: Timestamp) -> i64 {
    at.as_millisecond()
}

/// A stored Unix-millisecond column back as a timestamp.
pub(crate) fn from_millis(millis: i64) -> Result<Timestamp> {
    Timestamp::from_millisecond(millis)
        .map_err(|e| Error::corrupt(format!("timestamp {millis}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_round_trip_truncates_to_milliseconds() {
        let at: Timestamp = "2026-10-22T12:34:56.789123Z".parse().unwrap();
        let back = from_millis(to_millis(at)).unwrap();
        assert_eq!(
            back,
            "2026-10-22T12:34:56.789Z".parse::<Timestamp>().unwrap()
        );
    }

    #[test]
    fn out_of_range_millis_are_corrupt() {
        assert!(matches!(from_millis(i64::MAX), Err(Error::Corrupt(_))));
    }
}
