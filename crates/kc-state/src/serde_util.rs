//! Deserializers for Discord's loosely typed JSON.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A 64-bit integer that Discord sends as a decimal string (permission sets)
/// or occasionally as a number.
pub fn u64_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => text.parse().unwrap_or(0),
        Value::Number(number) => number.as_u64().unwrap_or(0),
        _ => 0,
    })
}

/// `null` and a missing field both become the type's default. Discord sends
/// explicit nulls for many list and string fields.
pub fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// An integer that may arrive as a number or a decimal string (folder ids).
pub fn opt_i64_lenient<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => number.as_i64(),
        _ => None,
    })
}
