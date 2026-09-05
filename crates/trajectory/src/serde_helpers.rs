//! Serde adapters for `std::time::SystemTime`. The default impl (as of
//! serde 1.0.166) produces `{secs, nanos}` which is awkward for JS;
//! these helpers produce/consume i64 milliseconds since the UNIX epoch.

pub mod system_time_ms {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let ms = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        s.serialize_i64(ms)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let ms = i64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64))
    }
}