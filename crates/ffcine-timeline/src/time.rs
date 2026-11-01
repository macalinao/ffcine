//! `RationalTime.1` and `TimeRange.1`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::schema;

/// A point in time: `value` frames at `rate` frames per second.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RationalTime {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "schema::rational_time")]
    pub otio_schema: String,
    pub rate: f64,
    pub value: f64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl RationalTime {
    pub fn new(value: f64, rate: f64) -> Self {
        Self {
            otio_schema: "RationalTime.1".into(),
            rate,
            value,
            extra: Map::new(),
        }
    }

    pub fn from_seconds(seconds: f64, rate: f64) -> Self {
        Self::new(seconds * rate, rate)
    }

    pub fn to_seconds(&self) -> f64 {
        if self.rate == 0.0 {
            0.0
        } else {
            self.value / self.rate
        }
    }

    /// This time at another rate (same instant, not rounded).
    #[must_use]
    pub fn rescaled_to(&self, rate: f64) -> Self {
        if self.rate.to_bits() == rate.to_bits() {
            return self.clone();
        }
        Self::from_seconds(self.to_seconds(), rate)
    }
}

impl Default for RationalTime {
    fn default() -> Self {
        Self::new(0.0, 1.0)
    }
}

/// `[start_time, start_time + duration)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeRange {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "schema::time_range")]
    pub otio_schema: String,
    pub duration: RationalTime,
    pub start_time: RationalTime,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl TimeRange {
    pub fn new(start_time: RationalTime, duration: RationalTime) -> Self {
        Self {
            otio_schema: "TimeRange.1".into(),
            duration,
            start_time,
            extra: Map::new(),
        }
    }

    /// `start..end` in frames at `rate`.
    pub fn from_frames(start: f64, end: f64, rate: f64) -> Self {
        Self::new(
            RationalTime::new(start, rate),
            RationalTime::new(end - start, rate),
        )
    }

    pub fn end_time_exclusive(&self) -> RationalTime {
        let d = self.duration.rescaled_to(self.start_time.rate);
        RationalTime::new(self.start_time.value + d.value, self.start_time.rate)
    }
}

impl Default for TimeRange {
    fn default() -> Self {
        Self::new(RationalTime::default(), RationalTime::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_time_rescales_duration() {
        let r = TimeRange::new(RationalTime::new(24.0, 24.0), RationalTime::new(50.0, 50.0));
        assert!((r.end_time_exclusive().value - 48.0).abs() < 1e-9);
        assert!((r.end_time_exclusive().to_seconds() - 2.0).abs() < 1e-9);
    }
}
