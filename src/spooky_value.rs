use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};
use serde_json::json;
use smol_str::SmolStr;
use std::convert::TryFrom;
use std::hash::BuildHasherDefault;

pub type FastMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SpookyNumber {
    I64(i64),
    U64(u64),
    F64(f64),
}

impl SpookyNumber {
    pub fn as_f64(self) -> f64 {
        match self {
            SpookyNumber::I64(i) => i as f64,
            SpookyNumber::U64(u) => u as f64,
            SpookyNumber::F64(f) => f,
        }
    }

    pub fn as_i64(self) -> Option<i64> {
        match self {
            SpookyNumber::I64(i) => Some(i),
            SpookyNumber::U64(u) => i64::try_from(u).ok(),
            SpookyNumber::F64(f) => {
                if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                    Some(f as i64)
                } else {
                    None
                }
            }
        }
    }

    pub fn as_u64(self) -> Option<u64> {
        match self {
            SpookyNumber::U64(u) => Some(u),
            SpookyNumber::I64(i) => u64::try_from(i).ok(),
            SpookyNumber::F64(f) => {
                if f.fract() == 0.0 && f >= 0.0 && f <= u64::MAX as f64 {
                    Some(f as u64)
                } else {
                    None
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SpookyValue {
    Null,
    Bool(bool),
    Number(SpookyNumber),
    Str(SmolStr),
    Array(Vec<SpookyValue>),
    Object(FastMap<SmolStr, SpookyValue>),
}

impl Default for SpookyValue {
    fn default() -> Self {
        SpookyValue::Null
    }
}

impl SpookyValue {
    /// Get value as string reference
    pub fn as_str(&self) -> Option<&str> {
        match self {
            SpookyValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Get value as f64
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SpookyValue::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            SpookyValue::Number(n) => n.as_i64(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            SpookyValue::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    /// Get value as bool
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SpookyValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Get value as object reference
    pub fn as_object(&self) -> Option<&FastMap<SmolStr, SpookyValue>> {
        match self {
            SpookyValue::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Get value as array reference
    pub fn as_array(&self) -> Option<&Vec<SpookyValue>> {
        match self {
            SpookyValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    /// Get nested value by key (for objects)
    pub fn get(&self, key: &str) -> Option<&SpookyValue> {
        self.as_object()?.get(&SmolStr::new(key))
    }

    /// Check if value is null
    pub fn is_null(&self) -> bool {
        matches!(self, SpookyValue::Null)
    }
}

impl From<f64> for SpookyValue {
    fn from(n: f64) -> Self {
        SpookyValue::Number(SpookyNumber::F64(n))
    }
}

impl From<i64> for SpookyValue {
    fn from(n: i64) -> Self {
        SpookyValue::Number(SpookyNumber::I64(n))
    }
}

impl From<u64> for SpookyValue {
    fn from(n: u64) -> Self {
        SpookyValue::Number(SpookyNumber::U64(n))
    }
}

impl From<bool> for SpookyValue {
    fn from(b: bool) -> Self {
        SpookyValue::Bool(b)
    }
}

impl From<&str> for SpookyValue {
    fn from(s: &str) -> Self {
        SpookyValue::Str(SmolStr::from(s))
    }
}

impl From<String> for SpookyValue {
    fn from(s: String) -> Self {
        SpookyValue::Str(SmolStr::from(s))
    }
}

impl From<serde_json::Value> for SpookyValue {
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => SpookyValue::Null,
            serde_json::Value::Bool(b) => SpookyValue::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    SpookyValue::Number(SpookyNumber::I64(i))
                } else if let Some(u) = n.as_u64() {
                    SpookyValue::Number(SpookyNumber::U64(u))
                } else {
                    SpookyValue::Number(SpookyNumber::F64(n.as_f64().unwrap_or(0.0)))
                }
            }
            serde_json::Value::String(s) => SpookyValue::Str(SmolStr::from(s)),
            serde_json::Value::Array(arr) => {
                SpookyValue::Array(arr.into_iter().map(SpookyValue::from).collect())
            }
            serde_json::Value::Object(obj) => SpookyValue::Object(
                obj.into_iter()
                    .map(|(k, v)| (SmolStr::from(k), SpookyValue::from(v)))
                    .collect(),
            ),
        }
    }
}

impl From<ciborium::Value> for SpookyValue {
    fn from(v: ciborium::Value) -> Self {
        match v {
            ciborium::Value::Null => SpookyValue::Null,
            ciborium::Value::Bool(b) => SpookyValue::Bool(b),
            ciborium::Value::Integer(i) => {
                if let Ok(val_i64) = i64::try_from(i) {
                    SpookyValue::Number(SpookyNumber::I64(val_i64))
                } else if let Ok(val_u64) = u64::try_from(i) {
                    SpookyValue::Number(SpookyNumber::U64(val_u64))
                } else {
                    let val_huge = i128::try_from(i).unwrap_or(0);
                    SpookyValue::Number(SpookyNumber::F64(val_huge as f64))
                }
            }
            ciborium::Value::Float(f) => SpookyValue::Number(SpookyNumber::F64(f)),
            ciborium::Value::Text(s) => SpookyValue::Str(SmolStr::from(s)),
            ciborium::Value::Array(arr) => {
                SpookyValue::Array(arr.into_iter().map(SpookyValue::from).collect())
            }
            ciborium::Value::Map(map) => SpookyValue::Object(
                map.into_iter()
                    .map(|(k, v)| {
                        let key_str = match k {
                            ciborium::value::Value::Text(s) => SmolStr::from(s),
                            ciborium::value::Value::Integer(i) => {
                                let val = i128::try_from(i).unwrap_or(0);
                                SmolStr::from(val.to_string())
                            }
                            other => SmolStr::from(format!("{:?}", other)),
                        };
                        (key_str, SpookyValue::from(v))
                    })
                    .collect(),
            ),
            // Covers Bytes, Tag, and any future ciborium variants
            _ => SpookyValue::Null,
        }
    }
}

impl From<SpookyValue> for ciborium::Value {
    fn from(val: SpookyValue) -> Self {
        match val {
            SpookyValue::Null => ciborium::Value::Null,
            SpookyValue::Bool(b) => ciborium::Value::Bool(b),
            SpookyValue::Number(n) => match n {
                SpookyNumber::I64(i) => ciborium::Value::Integer(i.into()),
                SpookyNumber::U64(u) => ciborium::Value::Integer(u.into()),
                SpookyNumber::F64(f) => ciborium::Value::Float(f),
            },
            SpookyValue::Str(s) => ciborium::Value::Text(s.to_string()),
            SpookyValue::Array(arr) => {
                ciborium::Value::Array(arr.into_iter().map(|v| v.into()).collect())
            }
            SpookyValue::Object(obj) => ciborium::Value::Map(
                obj.into_iter()
                    .map(|(k, v)| (ciborium::Value::Text(k.to_string()), v.into()))
                    .collect(),
            ),
        }
    }
}

impl From<SpookyValue> for serde_json::Value {
    fn from(val: SpookyValue) -> Self {
        match val {
            SpookyValue::Null => serde_json::Value::Null,
            SpookyValue::Bool(b) => serde_json::Value::Bool(b),
            SpookyValue::Number(n) => match n {
                SpookyNumber::I64(i) => json!(i),
                SpookyNumber::U64(u) => json!(u),
                SpookyNumber::F64(f) => json!(f),
            },
            SpookyValue::Str(s) => serde_json::Value::String(s.to_string()),
            SpookyValue::Array(arr) => {
                serde_json::Value::Array(arr.into_iter().map(|v| v.into()).collect())
            }
            SpookyValue::Object(obj) => serde_json::Value::Object(
                obj.into_iter()
                    .map(|(k, v)| (k.to_string(), v.into()))
                    .collect(),
            ),
        }
    }
}

#[macro_export]
macro_rules! spooky_obj {
    // Einstiegspunkt für Objekte
    ({ $($key:expr => $val:tt),* $(,)? }) => {{
        let mut map = FastMap::default();
        $(
            map.insert(
                SmolStr::new($key),
                SpookyValue::from(spooky_obj!(@value $val))
            );
        )*
        SpookyValue::Object(map)
    }};

    // Rekursion für verschachtelte Objekte
    (@value { $($inner:tt)* }) => {
        spooky_obj!({ $($inner)* })
    };

    // Fallback für alles andere (Literale oder fertige SpookyValues)
    (@value $val:expr) => {
        $val
    };
}
