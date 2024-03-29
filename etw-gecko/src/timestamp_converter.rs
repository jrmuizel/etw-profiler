use fxprof_processed_profile::Timestamp;

#[derive(Debug, Clone, Copy)]
pub struct TimestampConverter {
    /// A reference timestamp, as a raw timestamp.
    pub reference_raw: u64,
    /// A "ticks per nanosecond" conversion factor. If raw values are in nanoseconds, this is 1.
    pub raw_to_ns_factor: u64,
}

impl TimestampConverter {
    pub fn convert_raw(&self, raw: u64) -> Timestamp {
        Timestamp::from_nanos_since_reference(
            raw.saturating_sub(self.reference_raw) * self.raw_to_ns_factor,
        )
    }

    pub fn convert_us(&self, time_us: u64) -> Timestamp {
        Timestamp::from_nanos_since_reference(
            (time_us * 1000).saturating_sub(self.reference_raw * self.raw_to_ns_factor),
        )
    }
}
