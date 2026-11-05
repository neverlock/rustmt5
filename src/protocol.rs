//! Binary protocol: framing, dynamic [`Value`]/[`Record`] types and the schema driven
//! [`SeriesCodec`] (port of `pymt5.protocol`).

use std::ops::{Deref, DerefMut};

use indexmap::IndexMap;

use crate::constants::{HEADER_BYTE_LENGTH, OUTER_PROTOCOL_VERSION};
use crate::error::{Error, Result};
use crate::helpers::{decode_utf16le, encode_utf16le, pad_ascii, pad_bytes, random_command_prefix, strip_fixed_string};

/// Wire type of a schema field (matches the `PROP_*` ids of the Python library).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropType {
    I8, I16, I32, U8, U16, U32, F32, F64, Time, I64, U64, String, FixedString, Bytes,
}

impl PropType {
    /// Numeric `PROP_*` id used by the JS bundle.
    pub fn id(self) -> u8 {
        use PropType::*;
        match self {
            I8 => 1, I16 => 2, I32 => 3, U8 => 4, U16 => 5, U32 => 6, F32 => 7, F64 => 8, Time => 9,
            String => 10, FixedString => 11, Bytes => 12, I64 => 17, U64 => 18,
        }
    }
}

/// One entry of a schema: type plus optional byte length (strings/bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub ty: PropType,
    pub len: Option<usize>,
}

/// Dynamically typed parsed value (mirrors the Python `Any` values).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Record(Record),
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            Value::UInt(v) => i64::try_from(*v).ok(),
            Value::Float(v) => Some(*v as i64),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::UInt(v) => Some(*v),
            Value::Int(v) => u64::try_from(*v).ok(),
            Value::Float(v) if *v >= 0.0 => Some(*v as u64),
            Value::Bool(b) => Some(*b as u64),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(v) => Some(*v),
            Value::Int(v) => Some(*v as f64),
            Value::UInt(v) => Some(*v as f64),
            Value::Bool(b) => Some(*b as u8 as f64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Null => Some(false),
            other => other.as_f64().map(|v| v != 0.0),
        }
    }
    pub fn as_str(&self) -> Option<&str> { if let Value::Str(s) = self { Some(s) } else { None } }
    pub fn as_bytes(&self) -> Option<&[u8]> { if let Value::Bytes(b) = self { Some(b) } else { None } }
    pub fn as_list(&self) -> Option<&[Value]> { if let Value::List(l) = self { Some(l) } else { None } }
    pub fn as_record(&self) -> Option<&Record> { if let Value::Record(r) = self { Some(r) } else { None } }
}

macro_rules! value_from {
    ($($t:ty => $variant:ident as $cast:ty),* $(,)?) => {$(
        impl From<$t> for Value { fn from(v: $t) -> Self { Value::$variant(v as $cast) } }
    )*};
}
value_from!(i8 => Int as i64, i16 => Int as i64, i32 => Int as i64, i64 => Int as i64, isize => Int as i64,
    u8 => UInt as u64, u16 => UInt as u64, u32 => UInt as u64, u64 => UInt as u64, usize => UInt as u64,
    f32 => Float as f64, f64 => Float as f64);
impl From<bool> for Value { fn from(v: bool) -> Self { Value::Bool(v) } }
impl From<&str> for Value { fn from(v: &str) -> Self { Value::Str(v.to_string()) } }
impl From<String> for Value { fn from(v: String) -> Self { Value::Str(v) } }
impl From<Vec<u8>> for Value { fn from(v: Vec<u8>) -> Self { Value::Bytes(v) } }
impl From<Record> for Value { fn from(v: Record) -> Self { Value::Record(v) } }
impl From<Vec<Record>> for Value { fn from(v: Vec<Record>) -> Self { Value::List(v.into_iter().map(Value::Record).collect()) } }
impl From<Vec<f64>> for Value { fn from(v: Vec<f64>) -> Self { Value::List(v.into_iter().map(Value::Float).collect()) } }

/// Insertion ordered `name -> Value` map (the Rust equivalent of the Python `dict` records).
///
/// The `get_*` helpers follow Python's `x.get(k, 0) or 0` idiom: a missing key or
/// mismatched type yields the zero value instead of an error.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Record(pub IndexMap<String, Value>);

pub type RecordList = Vec<Record>;

impl Deref for Record { type Target = IndexMap<String, Value>; fn deref(&self) -> &Self::Target { &self.0 } }
impl DerefMut for Record { fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 } }

impl Record {
    pub fn new() -> Self { Self::default() }
    /// Builder style insert.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.0.insert(key.to_string(), value.into());
        self
    }
    pub fn set(&mut self, key: &str, value: impl Into<Value>) { self.0.insert(key.to_string(), value.into()); }
    pub fn get_i64(&self, key: &str) -> i64 { self.0.get(key).and_then(Value::as_i64).unwrap_or(0) }
    pub fn get_u64(&self, key: &str) -> u64 { self.0.get(key).and_then(Value::as_u64).unwrap_or(0) }
    pub fn get_f64(&self, key: &str) -> f64 { self.0.get(key).and_then(Value::as_f64).unwrap_or(0.0) }
    pub fn get_bool(&self, key: &str) -> bool { self.0.get(key).and_then(Value::as_bool).unwrap_or(false) }
    pub fn get_str(&self, key: &str) -> &str { self.0.get(key).and_then(Value::as_str).unwrap_or("") }
    pub fn get_record(&self, key: &str) -> Option<&Record> { self.0.get(key).and_then(Value::as_record) }
    pub fn get_list(&self, key: &str) -> &[Value] { self.0.get(key).and_then(Value::as_list).unwrap_or(&[]) }
    /// `list` of records stored under `key` (non-record items skipped).
    pub fn get_records(&self, key: &str) -> Vec<&Record> { self.get_list(key).iter().filter_map(Value::as_record).collect() }
    /// Zip `names` with parsed `values` (like `dict(zip(names, values))`).
    pub fn from_zip(names: &[&str], values: Vec<Value>) -> Self {
        Record(names.iter().map(|n| n.to_string()).zip(values).collect())
    }
}

/// Parsed command response frame.
#[derive(Debug, Clone)]
pub struct ResponseFrame {
    pub command: u16,
    pub code: u8,
    pub body: Vec<u8>,
}

const FILETIME_EPOCH_OFFSET_MS: i128 = 11_644_473_600_000;
const FILETIME_TICKS_PER_MS: i128 = 10_000;

fn unix_ms_to_filetime(unix_ms: i64) -> Result<[u8; 8]> {
    let ft = (unix_ms as i128 + FILETIME_EPOCH_OFFSET_MS) * FILETIME_TICKS_PER_MS;
    u64::try_from(ft).map(|v| v.to_le_bytes()).map_err(|_| Error::protocol("timestamp out of range"))
}

fn filetime_to_unix_ms(buf: &[u8]) -> i64 {
    let ft = u64::from_le_bytes(buf[..8].try_into().expect("8 bytes")) as i128;
    (ft / FILETIME_TICKS_PER_MS - FILETIME_EPOCH_OFFSET_MS) as i64
}

/// Wrap an encrypted body in the 8-byte outer header.
pub fn pack_outer(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTE_LENGTH + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&OUTER_PROTOCOL_VERSION.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Split an outer frame into `(body_len, version, body)`.
pub fn unpack_outer(frame: &[u8]) -> Result<(u32, u32, &[u8])> {
    if frame.len() < HEADER_BYTE_LENGTH {
        return Err(Error::protocol("frame too short"));
    }
    let body_len = u32::from_le_bytes(frame[0..4].try_into().unwrap());
    let version = u32::from_le_bytes(frame[4..8].try_into().unwrap());
    let body = &frame[HEADER_BYTE_LENGTH..];
    if body_len as usize != body.len() {
        return Err(Error::protocol(format!("frame length mismatch: header={} actual={}", body_len, body.len())));
    }
    Ok((body_len, version, body))
}

/// `random(2) + command(u16 LE) + payload`.
pub fn build_command(command: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&random_command_prefix());
    out.extend_from_slice(&command.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn parse_response_frame(data: &[u8]) -> Result<ResponseFrame> {
    if data.len() < 5 {
        return Err(Error::protocol("response frame too short"));
    }
    Ok(ResponseFrame { command: u16::from_le_bytes([data[2], data[3]]), code: data[4], body: data[5..].to_vec() })
}

/// Total byte size of a schema.
pub fn get_series_size(schema: &[Field]) -> Result<usize> {
    let mut size = 0;
    for f in schema {
        size += match f.ty {
            PropType::I8 | PropType::U8 => 1,
            PropType::I16 | PropType::U16 => 2,
            PropType::I32 | PropType::U32 | PropType::F32 => 4,
            PropType::F64 | PropType::Time | PropType::I64 | PropType::U64 => 8,
            PropType::FixedString | PropType::Bytes | PropType::String => {
                f.len.ok_or_else(|| Error::protocol(format!("propLength required for propType={}", f.ty.id())))?
            }
        };
    }
    Ok(size)
}

/// A value to serialize: type, value and optional length.
#[derive(Debug, Clone)]
pub struct SerField {
    pub ty: PropType,
    pub value: Value,
    pub len: Option<usize>,
}

impl SerField {
    pub fn new(ty: PropType, value: impl Into<Value>, len: Option<usize>) -> Self { Self { ty, value: value.into(), len } }
    pub fn u8(v: u8) -> Self { Self::new(PropType::U8, v, None) }
    pub fn i16(v: i16) -> Self { Self::new(PropType::I16, v, None) }
    pub fn u16(v: u16) -> Self { Self::new(PropType::U16, v, None) }
    pub fn i32(v: i32) -> Self { Self::new(PropType::I32, v, None) }
    pub fn u32(v: u32) -> Self { Self::new(PropType::U32, v, None) }
    pub fn u64(v: u64) -> Self { Self::new(PropType::U64, v, None) }
    pub fn f64(v: f64) -> Self { Self::new(PropType::F64, v, None) }
    /// Unix milliseconds encoded as Windows FILETIME.
    pub fn time_ms(v: i64) -> Self { Self::new(PropType::Time, v, None) }
    pub fn fixed_string(s: &str, len: usize) -> Self { Self::new(PropType::FixedString, s, Some(len)) }
    /// Raw bytes padded/truncated to `len`.
    pub fn bytes(b: &[u8], len: usize) -> Self { Self::new(PropType::Bytes, b.to_vec(), Some(len)) }
    /// Raw bytes appended as-is (no padding).
    pub fn raw(b: &[u8]) -> Self { Self::new(PropType::Bytes, b.to_vec(), None) }
}

macro_rules! pack_int {
    ($v:expr, $t:ty, $name:expr) => {{
        let n = $v.as_i128()?;
        <$t>::try_from(n).map_err(|_| Error::protocol(format!("{} value out of range: {}", $name, n)))?.to_le_bytes().to_vec()
    }};
}

trait AsI128 { fn as_i128(&self) -> Result<i128>; }
impl AsI128 for Value {
    fn as_i128(&self) -> Result<i128> {
        match self {
            Value::Int(v) => Ok(*v as i128),
            Value::UInt(v) => Ok(*v as i128),
            Value::Float(v) => Ok(*v as i128),
            Value::Bool(b) => Ok(*b as i128),
            Value::Null => Ok(0),
            _ => Err(Error::protocol("expected numeric value")),
        }
    }
}

/// Schema driven binary (de)serializer.
pub struct SeriesCodec;

impl SeriesCodec {
    pub fn serialize(fields: &[SerField]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for f in fields {
            let v = &f.value;
            let chunk: Vec<u8> = match f.ty {
                PropType::I8 => pack_int!(v, i8, "I8"),
                PropType::U8 => pack_int!(v, u8, "U8"),
                PropType::I16 => pack_int!(v, i16, "I16"),
                PropType::U16 => pack_int!(v, u16, "U16"),
                PropType::I32 => pack_int!(v, i32, "I32"),
                PropType::U32 => pack_int!(v, u32, "U32"),
                PropType::I64 => pack_int!(v, i64, "I64"),
                PropType::U64 => pack_int!(v, u64, "U64"),
                PropType::F32 => (v.as_f64().unwrap_or(0.0) as f32).to_le_bytes().to_vec(),
                PropType::F64 => v.as_f64().unwrap_or(0.0).to_le_bytes().to_vec(),
                PropType::Time => unix_ms_to_filetime(v.as_i64().unwrap_or(0))?.to_vec(),
                PropType::FixedString => {
                    let len = f.len.ok_or_else(|| Error::protocol("fixed string requires propLength"))?;
                    encode_utf16le(v.as_str().unwrap_or(""), len)
                }
                PropType::Bytes => {
                    let raw = v.as_bytes().unwrap_or(&[]);
                    match f.len { None => raw.to_vec(), Some(n) => pad_bytes(raw, n) }
                }
                PropType::String => {
                    let len = f.len.ok_or_else(|| Error::protocol("string requires propLength in experimental codec"))?;
                    pad_ascii(v.as_str().unwrap_or(""), len)
                }
            };
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Parse `schema` from `buffer[offset..]`.
    pub fn parse(buffer: &[u8], schema: &[Field], offset: usize) -> Result<Vec<Value>> {
        let required = get_series_size(schema)?;
        let available = buffer.len().saturating_sub(offset);
        if available < required {
            return Err(Error::protocol(format!(
                "buffer too short: need {required} bytes from offset {offset}, but only {available} bytes available"
            )));
        }
        let mut values = Vec::with_capacity(schema.len());
        let mut c = offset;
        macro_rules! take { ($n:expr) => {{ let s = &buffer[c..c + $n]; c += $n; s }}; }
        for f in schema {
            let v = match f.ty {
                PropType::I8 => Value::Int(i8::from_le_bytes(take!(1).try_into().unwrap()) as i64),
                PropType::U8 => Value::UInt(take!(1)[0] as u64),
                PropType::I16 => Value::Int(i16::from_le_bytes(take!(2).try_into().unwrap()) as i64),
                PropType::U16 => Value::UInt(u16::from_le_bytes(take!(2).try_into().unwrap()) as u64),
                PropType::I32 => Value::Int(i32::from_le_bytes(take!(4).try_into().unwrap()) as i64),
                PropType::U32 => Value::UInt(u32::from_le_bytes(take!(4).try_into().unwrap()) as u64),
                PropType::F32 => Value::Float(f32::from_le_bytes(take!(4).try_into().unwrap()) as f64),
                PropType::F64 => Value::Float(f64::from_le_bytes(take!(8).try_into().unwrap())),
                PropType::Time => Value::Int(filetime_to_unix_ms(take!(8))),
                PropType::I64 => Value::Int(i64::from_le_bytes(take!(8).try_into().unwrap())),
                PropType::U64 => Value::UInt(u64::from_le_bytes(take!(8).try_into().unwrap())),
                PropType::FixedString => Value::Str(decode_utf16le(take!(f.len.unwrap()))),
                PropType::Bytes => Value::Bytes(take!(f.len.unwrap()).to_vec()),
                PropType::String => Value::Str(strip_fixed_string(take!(f.len.unwrap()))),
            };
            values.push(v);
        }
        Ok(values)
    }

    /// Alias of [`SeriesCodec::parse`] (kept for parity with the Python API).
    pub fn parse_at(buffer: &[u8], schema: &[Field], offset: usize) -> Result<Vec<Value>> { Self::parse(buffer, schema, offset) }

    /// Parse and zip with `names` into a [`Record`].
    pub fn parse_record(buffer: &[u8], schema: &[Field], names: &[&str], offset: usize) -> Result<Record> {
        Ok(Record::from_zip(names, Self::parse(buffer, schema, offset)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::*;

    #[test]
    fn schema_sizes_match_js_layouts() {
        assert_eq!(get_series_size(SYMBOL_BASIC_SCHEMA).unwrap(), 526);
        assert_eq!(get_series_size(TICK_SCHEMA).unwrap(), 50);
        assert_eq!(get_series_size(RATE_BAR_SCHEMA).unwrap(), 48);
        assert_eq!(get_series_size(POSITION_SCHEMA).unwrap(), 8 + 8 + 4 + 4 + 64 + 4 + 8 * 4 + 8 + 8 * 5 + 8 + 8 + 64 + 8 + 4 * 3 + 64 + 4 + 4);
        assert_eq!(get_series_size(ORDER_SCHEMA).unwrap(), 8 + 64 + 64 + 4 * 7 + 8 * 5 + 8 * 2 + 4 + 8 * 2 + 64 + 8 + 4 * 2 + 8 * 3 + 4 + 4 * 2);
        assert_eq!(get_series_size(DEAL_SCHEMA).unwrap(), 8 + 64 + 8 + 4 * 2 + 64 + 4 * 2 + 8 * 4 + 8 + 8 * 5 + 8 * 2 + 64 + 8 + 4 * 3 + 4 * 2 + 8);
    }

    #[test]
    fn outer_framing() {
        let f = pack_outer(b"hello");
        let (len, ver, body) = unpack_outer(&f).unwrap();
        assert_eq!((len, ver, body), (5, 1, &b"hello"[..]));
        assert!(unpack_outer(&[0, 1, 2]).unwrap_err().to_string().contains("frame too short"));
        let mut bad = pack_outer(b"abcd");
        bad[0] = 100;
        assert!(unpack_outer(&bad).unwrap_err().to_string().contains("frame length mismatch"));
        assert_eq!(unpack_outer(&pack_outer(b"")).unwrap().2, b"");
    }

    #[test]
    fn response_frame() {
        let f = parse_response_frame(&[0xAA, 0xBB, 0x1C, 0x00, 0x05, b'h', b'i']).unwrap();
        assert_eq!((f.command, f.code, f.body.as_slice()), (28, 5, &b"hi"[..]));
        assert!(parse_response_frame(&[0, 1, 2, 3]).is_err());
        assert_eq!(parse_response_frame(&[0xAA, 0xBB, 0, 0, 5]).unwrap().body.len(), 0);
    }

    #[test]
    fn time_roundtrip() {
        let ms = 1_710_501_234_567i64;
        let p = SeriesCodec::serialize(&[SerField::time_ms(ms)]).unwrap();
        assert_eq!(p.len(), 8);
        let v = SeriesCodec::parse(&p, &[Field { ty: PropType::Time, len: None }], 0).unwrap();
        assert_eq!(v, vec![Value::Int(ms)]);
    }

    #[test]
    fn symbol_basic_roundtrip() {
        let data = SeriesCodec::serialize(&[
            SerField::fixed_string("EURUSD", 64),
            SerField::fixed_string("Forex\\Major", 128),
            SerField::u32(5),
            SerField::u32(42),
            SerField::fixed_string("Euro vs US Dollar", 256),
            SerField::u32(0),
            SerField::fixed_string("USD", 64),
            SerField::u16(1),
        ])
        .unwrap();
        assert_eq!(data.len(), 526);
        let r = SeriesCodec::parse_record(&data, SYMBOL_BASIC_SCHEMA, SYMBOL_BASIC_FIELD_NAMES, 0).unwrap();
        assert_eq!(r.get_str("trade_symbol"), "EURUSD");
        assert_eq!(r.get_i64("digits"), 5);
        assert_eq!(r.get_u64("symbol_id"), 42);
    }

    #[test]
    fn truncated_buffer_errors() {
        let e = SeriesCodec::parse(&[0; 10], TICK_SCHEMA, 0).unwrap_err().to_string();
        assert!(e.contains("buffer too short"));
        assert!(SeriesCodec::parse(&[0; 50], TICK_SCHEMA, 45).is_err());
    }

    #[test]
    fn out_of_range_int_rejected() {
        assert!(SeriesCodec::serialize(&[SerField::new(PropType::U8, 300u32, None)]).is_err());
        assert!(SeriesCodec::serialize(&[SerField::new(PropType::U32, -1i64, None)]).is_err());
    }
}
