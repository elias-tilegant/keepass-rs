//! Custom serde (de)serializers for specific data formats in KeePass XML flavor.

/// base64-encoded binary data
pub mod cs_base64 {
    use base64::{engine::general_purpose as base64_engine, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(data: &[u8], s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&base64_engine::STANDARD.encode(data))
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;

        base64_engine::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

/// "True"/"False" boolean strings
pub mod cs_bool {

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(data: &bool, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(if *data { "True" } else { "False" })
    }

    pub fn deserialize<'de, D>(d: D) -> Result<bool, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;

        match s.as_str().to_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(serde::de::Error::custom(format!("Invalid boolean string: {}", s))),
        }
    }
}

/// Optional "True"/"False" boolean strings
pub mod cs_opt_bool {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(data: &Option<bool>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match data {
            Some(b) => s.serialize_str(if *b { "True" } else { "False" }),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Option<bool>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<String>::deserialize(d)?;

        match opt {
            Some(s) => {
                if s.trim().is_empty() {
                    Ok(None)
                } else {
                    match s.to_lowercase().as_str() {
                        "true" | "1" => Ok(Some(true)),
                        "false" | "0" => Ok(Some(false)),
                        "null" => Ok(None),
                        _ => Err(serde::de::Error::custom(format!("Invalid boolean string: {}", s))),
                    }
                }
            }
            None => Ok(None),
        }
    }
}

/// Optional value that implements FromStr that may be missing empty, e.g. numbers
pub mod cs_opt_fromstr {
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, T>(data: &Option<T>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match data {
            Some(v) => s.serialize_some(v),
            None => s.serialize_str(""), // this will make quick-xml serialize as <Tag></Tag>
        }
    }

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        <T as FromStr>::Err: std::fmt::Display,
    {
        if let Some(s) = Option::<String>::deserialize(d)? {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                let n: T = s
                    .parse()
                    .map_err(|e| serde::de::Error::custom(format!("error parsing: {}", e)))?;
                Ok(Some(n))
            }
        } else {
            Ok(None)
        }
    }
}

/// `AutoType/DataTransferObfuscation` — KeePass's XSD declares this as
/// `xs:int` (0 or 1), not `xs:boolean`. Writing `True`/`False` causes
/// KeePassXC to reject the file with "invalid number value". This serde
/// helper writes 0/1 on save and accepts both number- and boolean-shaped
/// inputs on read for tolerance with whatever earlier writers produced.
pub mod cs_opt_autotype_obfuscation {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(data: &Option<bool>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match data {
            Some(false) => s.serialize_str("0"),
            Some(true) => s.serialize_str("1"),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Option<bool>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<String>::deserialize(d)?
            .as_deref()
            .map(str::trim)
        {
            None | Some("") | Some("Null") | Some("null") => Ok(None),
            Some("0") | Some("False") | Some("false") => Ok(Some(false)),
            Some("1") | Some("True") | Some("true") => Ok(Some(true)),
            Some(v) => Err(serde::de::Error::custom(format!(
                "invalid DataTransferObfuscation value: {v}"
            ))),
        }
    }
}

/// Optional stringly value that may be missing or empty
/// (e.g. `<Name></Name>` or `<Name/>`)
pub mod cs_opt_string {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, T>(data: &Option<T>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match data {
            Some(v) => s.serialize_some(v),
            None => s.serialize_str(""), // this will make quick-xml serialize as <Tag></Tag>
        }
    }

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        if let Some(s) = Option::<String>::deserialize(d)? {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                let v = T::deserialize(serde::de::IntoDeserializer::<D::Error>::into_deserializer(s))
                    .map_err(serde::de::Error::custom)?;
                Ok(Some(v))
            }
        } else {
            Ok(None)
        }
    }
}
