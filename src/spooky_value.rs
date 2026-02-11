use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use smol_str::SmolStr;
use std::collections::BTreeMap;
use std::convert::TryFrom;

pub type FastMap<K, V> = BTreeMap<K, V>;

// ─── SpookyNumber ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum SpookyNumber {
    I64(i64),
    U64(u64),
    F64(f64),
}

impl std::fmt::Debug for SpookyNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpookyNumber::I64(i) => write!(f, "I64({})", i),
            SpookyNumber::U64(u) => write!(f, "U64({})", u),
            SpookyNumber::F64(v) => write!(f, "F64({})", v),
        }
    }
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

// ─── SpookyValue ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
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
    pub fn as_str(&self) -> Option<&str> {
        match self {
            SpookyValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

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

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            SpookyValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&FastMap<SmolStr, SpookyValue>> {
        match self {
            SpookyValue::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<SpookyValue>> {
        match self {
            SpookyValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&SpookyValue> {
        self.as_object()?.get(&SmolStr::new(key))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, SpookyValue::Null)
    }
}

// ─── Serialize (for ciborium::into_writer on nested types) ──────────────────

impl Serialize for SpookyValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SpookyValue::Null => serializer.serialize_none(),
            SpookyValue::Bool(b) => serializer.serialize_bool(*b),
            SpookyValue::Number(n) => match n {
                SpookyNumber::I64(i) => serializer.serialize_i64(*i),
                SpookyNumber::U64(u) => serializer.serialize_u64(*u),
                SpookyNumber::F64(f) => serializer.serialize_f64(*f),
            },
            SpookyValue::Str(s) => serializer.serialize_str(s.as_str()),
            SpookyValue::Array(arr) => {
                let mut seq = serializer.serialize_seq(Some(arr.len()))?;
                for v in arr {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            SpookyValue::Object(map) => {
                let mut m = serializer.serialize_map(Some(map.len()))?;
                for (k, v) in map {
                    m.serialize_entry(k.as_str(), v)?;
                }
                m.end()
            }
        }
    }
}

// ─── From impls ─────────────────────────────────────────────────────────────

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

// ─── From<ciborium::Value> ─────────────────────────────────────────────────

impl From<ciborium::Value> for SpookyValue {
    fn from(v: ciborium::Value) -> Self {
        match v {
            ciborium::Value::Null => SpookyValue::Null,
            ciborium::Value::Bool(b) => SpookyValue::Bool(b),
            ciborium::Value::Integer(i) => {
                if let Ok(val) = i64::try_from(i) {
                    SpookyValue::Number(SpookyNumber::I64(val))
                } else if let Ok(val) = u64::try_from(i) {
                    SpookyValue::Number(SpookyNumber::U64(val))
                } else {
                    let val = i128::try_from(i).unwrap_or(0);
                    SpookyValue::Number(SpookyNumber::F64(val as f64))
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
                        let key = match k {
                            ciborium::Value::Text(s) => SmolStr::from(s),
                            ciborium::Value::Integer(i) => {
                                let val = i128::try_from(i).unwrap_or(0);
                                SmolStr::from(val.to_string())
                            }
                            other => SmolStr::from(format!("{:?}", other)),
                        };
                        (key, SpookyValue::from(v))
                    })
                    .collect(),
            ),
            _ => SpookyValue::Null,
        }
    }
}

// ─── Into<ciborium::Value> ──────────────────────────────────────────────────

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

// ─── From/Into serde_json::Value ────────────────────────────────────────────

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

impl From<SpookyValue> for serde_json::Value {
    fn from(val: SpookyValue) -> Self {
        match val {
            SpookyValue::Null => serde_json::Value::Null,
            SpookyValue::Bool(b) => serde_json::Value::Bool(b),
            SpookyValue::Number(n) => match n {
                SpookyNumber::I64(i) => serde_json::json!(i),
                SpookyNumber::U64(u) => serde_json::json!(u),
                SpookyNumber::F64(f) => serde_json::json!(f),
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
