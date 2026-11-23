//! Lenient field decoders: Apple sends `null` where an array or flag is usually present,
//! and fields such as editorial artwork change shape without notice.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// `null` decodes as the type's default.
pub(crate) fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

/// A value that does not decode becomes the type's default (`None`, an empty map)
/// instead of failing its resource.
pub(crate) fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = Value::deserialize(deserializer)?;
    Ok(T::deserialize(&value).unwrap_or_default())
}
