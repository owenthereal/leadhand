//! The harness's own JSON reading, for what the CLI writes; never the crate's library.
//!
//! - Strings are read through serde_json's `deserialize_bytes` and kept as the WTF-8 it
//!   returns, never validated: distinct JSON strings stay distinct, lone surrogates
//!   (`"\udcff"`) included. A string becomes OS bytes only when a test compares it with a
//!   real file name (`to_os`, Python's surrogateescape).
//! - Numbers keep their JSON text (`Num`); objects keep every member, in order, duplicates
//!   included. Values are found through `RawValue`, so nothing is decoded on the way.
//! - `dumps` writes a value as Python's `json.dumps` does (the fakes' logs and replayed
//!   events): `", "` and `": "` separators, ASCII escapes, floats as Python's `repr`, and an
//!   object's duplicate keys collapsed as `json.loads` collapses them.
//!
//! The fake binaries include this file too (`#[path]`), so it uses nothing from the harness.

use std::ffi::OsString;
use std::fmt;
use std::ops::Index;
use std::os::unix::ffi::OsStringExt;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

#[derive(Clone, PartialEq, Eq)]
pub enum Json {
    Null,
    Bool(bool),
    /// The number's JSON text, as written.
    Num(String),
    /// The string's WTF-8: UTF-8, with any lone surrogate as its 3-byte form.
    Str(Vec<u8>),
    Arr(Vec<Json>),
    /// Every member in order; a duplicate key appears twice.
    Obj(Vec<(Vec<u8>, Json)>),
}

// --------------------------------------------------------------------------
// Parsing

/// `text` as JSON; panics naming the error and the text when it is not.
#[track_caller]
pub fn parse(text: &[u8]) -> Json {
    try_parse(text).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(text)))
}

/// `text` as JSON. JSON text is UTF-8 (escapes carry anything else).
pub fn try_parse(text: &[u8]) -> Result<Json, String> {
    let text = std::str::from_utf8(text).map_err(|e| format!("not UTF-8: {e}"))?;
    let raw: &RawValue = serde_json::from_str(text).map_err(|e| e.to_string())?;
    from_raw(raw).map_err(|e| e.to_string())
}

fn from_raw(raw: &RawValue) -> serde_json::Result<Json> {
    let text = raw.get();
    Ok(match text.as_bytes().first() {
        Some(b'{') => Json::Obj(serde_json::Deserializer::from_str(text).deserialize_map(Members)?),
        Some(b'[') => {
            let items: Vec<&RawValue> = serde_json::from_str(text)?;
            Json::Arr(items.into_iter().map(from_raw).collect::<Result<_, _>>()?)
        }
        Some(b'"') => Json::Str(serde_json::Deserializer::from_str(text).deserialize_bytes(Bytes)?),
        Some(b't') => Json::Bool(true),
        Some(b'f') => Json::Bool(false),
        Some(b'n') => Json::Null,
        _ => Json::Num(text.to_string()),
    })
}

/// An object's members: keys through `deserialize_bytes`, values as raw JSON, read in turn.
struct Members;

impl<'de> Visitor<'de> for Members {
    type Value = Vec<(Vec<u8>, Json)>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut members = Vec::new();
        while let Some(key) = map.next_key_seed(BytesSeed)? {
            let raw: &'de RawValue = map.next_value()?;
            members.push((key, from_raw(raw).map_err(de::Error::custom)?));
        }
        Ok(members)
    }
}

struct BytesSeed;

impl<'de> DeserializeSeed<'de> for BytesSeed {
    type Value = Vec<u8>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Vec<u8>, D::Error> {
        d.deserialize_bytes(Bytes)
    }
}

/// A string as the WTF-8 bytes serde_json's `deserialize_bytes` gives.
struct Bytes;

impl<'de> Visitor<'de> for Bytes {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON string")
    }

    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
        Ok(v.to_vec())
    }

    fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<Vec<u8>, E> {
        Ok(v.to_vec())
    }

    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
        Ok(v)
    }
}

// --------------------------------------------------------------------------
// WTF-8 and OS bytes

/// The code points of WTF-8 `bytes` (a surrogate's 3-byte form decodes to the surrogate);
/// None when they are not WTF-8.
pub fn code_points(bytes: &[u8]) -> Option<Vec<u32>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let (len, init, min) = match b {
            0x00..=0x7f => (1, u32::from(b), 0),
            0xc2..=0xdf => (2, u32::from(b & 0x1f), 0x80),
            0xe0..=0xef => (3, u32::from(b & 0x0f), 0x800),
            0xf0..=0xf4 => (4, u32::from(b & 0x07), 0x10000),
            _ => return None,
        };
        let tail = bytes.get(i + 1..i + len)?;
        let mut cp = init;
        for &t in tail {
            if t & 0xc0 != 0x80 {
                return None;
            }
            cp = (cp << 6) | u32::from(t & 0x3f);
        }
        if cp < min || cp > 0x10ffff {
            return None;
        }
        out.push(cp);
        i += len;
    }
    Some(out)
}

fn push_code_point(cp: u32, out: &mut Vec<u8>) {
    match cp {
        0..=0x7f => out.push(cp as u8),
        0x80..=0x7ff => out.extend([0xc0 | (cp >> 6) as u8, 0x80 | (cp & 0x3f) as u8]),
        0x800..=0xffff => out.extend([
            0xe0 | (cp >> 12) as u8,
            0x80 | ((cp >> 6) & 0x3f) as u8,
            0x80 | (cp & 0x3f) as u8,
        ]),
        _ => out.extend([
            0xf0 | (cp >> 18) as u8,
            0x80 | ((cp >> 12) & 0x3f) as u8,
            0x80 | ((cp >> 6) & 0x3f) as u8,
            0x80 | (cp & 0x3f) as u8,
        ]),
    }
}

/// WTF-8 as OS bytes, as Python's surrogateescape encodes a str: U+DC80..U+DCFF stand for
/// the bytes 0x80..0xFF. None for any other lone surrogate (no file name holds one).
pub fn to_os(wtf8: &[u8]) -> Option<OsString> {
    let mut out = Vec::with_capacity(wtf8.len());
    for cp in code_points(wtf8)? {
        match cp {
            0xdc80..=0xdcff => out.push((cp - 0xdc00) as u8),
            0xd800..=0xdfff => return None,
            _ => push_code_point(cp, &mut out),
        }
    }
    Some(OsString::from_vec(out))
}

/// OS bytes as WTF-8, as Python's surrogateescape decodes them: UTF-8 as is, each other
/// byte as U+DC00 plus the byte.
pub fn from_os(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.extend_from_slice(chunk.valid().as_bytes());
        for &b in chunk.invalid() {
            push_code_point(0xdc00 + u32::from(b), &mut out);
        }
    }
    out
}

// --------------------------------------------------------------------------
// Writing, as Python's json.dumps

/// The text Python's `json.dumps` writes for the value `json.loads` makes of `v`.
pub fn dumps(v: &Json) -> String {
    let mut out = String::new();
    write_json(v, &mut out);
    out
}

fn write_json(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(text) => out.push_str(&number(text)),
        Json::Str(s) => quote(s, out),
        Json::Arr(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Json::Obj(members) => {
            out.push('{');
            for (n, (k, item)) in dict(members).into_iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                quote(k, out);
                out.push_str(": ");
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(v, separators=(",", ":"))`: `dumps` with no spaces.
pub fn dumps_compact(v: &Json) -> String {
    let mut out = String::new();
    write_layout(v, None, 0, &mut out);
    out
}

/// `json.dumps(v, indent=indent)`: each item on its own line, `indent` spaces deeper per
/// level, `","` and `": "` separators, and `[]` and `{}` for empty containers.
pub fn dumps_indent(v: &Json, indent: usize) -> String {
    let mut out = String::new();
    write_layout(v, Some(indent), 0, &mut out);
    out
}

/// `json.dumps` with compact separators (`indent` None) or an indented layout.
fn write_layout(v: &Json, indent: Option<usize>, depth: usize, out: &mut String) {
    let newline = |out: &mut String, depth: usize| {
        if let Some(n) = indent {
            out.push('\n');
            out.push_str(&" ".repeat(n * depth));
        }
    };
    let key_sep = if indent.is_some() { ": " } else { ":" };
    match v {
        Json::Arr(items) if !items.is_empty() => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                write_layout(item, indent, depth + 1, out);
            }
            newline(out, depth);
            out.push(']');
        }
        Json::Obj(members) if !members.is_empty() => {
            out.push('{');
            for (n, (k, item)) in dict(members).into_iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                newline(out, depth + 1);
                quote(k, out);
                out.push_str(key_sep);
                write_layout(item, indent, depth + 1, out);
            }
            newline(out, depth);
            out.push('}');
        }
        Json::Arr(_) => out.push_str("[]"),
        Json::Obj(_) => out.push_str("{}"),
        scalar => write_json(scalar, out),
    }
}

/// An object's members as a Python dict holds them: a repeated key keeps its first place
/// and its last value.
pub fn dict(members: &[(Vec<u8>, Json)]) -> Vec<(&[u8], &Json)> {
    let mut out: Vec<(&[u8], &Json)> = Vec::with_capacity(members.len());
    for (k, v) in members {
        match out.iter_mut().find(|(seen, _)| *seen == k.as_slice()) {
            Some(slot) => slot.1 = v,
            None => out.push((k, v)),
        }
    }
    out
}

/// A JSON number as Python writes it back: an integer exactly (`-0` is `0`), anything else
/// as a float.
fn number(text: &str) -> String {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        if digits.bytes().all(|b| b == b'0') {
            return "0".to_string();
        }
        return text.to_string();
    }
    match text.parse::<f64>() {
        Ok(f) => json_float(f),
        Err(_) => text.to_string(),
    }
}

/// A float as `json.dumps` writes it: `repr`, with NaN and Infinity spelled as JSON's
/// extension does.
pub fn json_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        (if f > 0.0 { "Infinity" } else { "-Infinity" }).to_string()
    } else {
        float_repr(f)
    }
}

/// Python's `repr` of a float: the shortest digits that round-trip, in fixed notation when
/// the decimal point falls within 16 places of the first digit (with `.0` when integral),
/// else as `1.5e-05` or `1e+16`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return (if f > 0.0 { "inf" } else { "-inf" }).to_string();
    }
    if f == 0.0 {
        return (if f.is_sign_negative() { "-0.0" } else { "0.0" }).to_string();
    }
    let sci = format!("{:e}", f.abs()); // shortest round-trip digits: "1.2345e-7"
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;
    let point = exp + 1; // the decimal point's place after the first digit
    let sign = if f < 0.0 { "-" } else { "" };
    if -4 < point && point <= 16 {
        if point <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
        } else if point >= n {
            format!("{sign}{digits}{}.0", "0".repeat((point - n) as usize))
        } else {
            let (int, frac) = digits.split_at(point as usize);
            format!("{sign}{int}.{frac}")
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_string()
        } else {
            format!("{first}.{rest}")
        };
        format!("{sign}{mantissa}e{exp:+03}")
    }
}

/// A string as `json.dumps` writes it (ensure_ascii): printable ASCII as is, `"` and `\`
/// escaped, `\b \f \n \r \t` short, everything else `\uxxxx` (a surrogate pair above
/// U+FFFF). Bytes that are not WTF-8 are written as surrogateescape would decode them.
fn quote(s: &[u8], out: &mut String) {
    let cps = code_points(s)
        .unwrap_or_else(|| code_points(&from_os(s)).expect("surrogateescape gives WTF-8"));
    out.push('"');
    for cp in cps {
        match cp {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x20..=0x7e => out.push(cp as u8 as char),
            0..=0xffff => out.push_str(&format!("\\u{cp:04x}")),
            _ => {
                let v = cp - 0x10000;
                out.push_str(&format!(
                    "\\u{:04x}\\u{:04x}",
                    0xd800 + (v >> 10),
                    0xdc00 + (v & 0x3ff)
                ));
            }
        }
    }
    out.push('"');
}

// --------------------------------------------------------------------------
// Python's equality

/// A JSON scalar as Python's numeric tower sees it: `bool` is an `int` (True == 1).
enum PyNum {
    /// An integer, as canonical decimal text (`-0` is `0`).
    Int(String),
    Float(f64),
}

fn py_num(v: &Json) -> Option<PyNum> {
    match v {
        Json::Bool(b) => Some(PyNum::Int(if *b { "1" } else { "0" }.to_string())),
        Json::Num(text) => {
            let digits = text.strip_prefix('-').unwrap_or(text);
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                Some(PyNum::Int(number(text)))
            } else {
                text.parse().ok().map(PyNum::Float)
            }
        }
        _ => None,
    }
}

fn num_eq(a: &PyNum, b: &PyNum) -> bool {
    match (a, b) {
        (PyNum::Int(x), PyNum::Int(y)) => x == y,
        (PyNum::Float(x), PyNum::Float(y)) => x == y,
        // Python compares an int with a float exactly.
        (PyNum::Int(i), PyNum::Float(f)) | (PyNum::Float(f), PyNum::Int(i)) => {
            f.is_finite() && f.fract() == 0.0 && number(&format!("{f:.0}")) == *i
        }
    }
}

/// Python's `==` on the values `json.loads` makes of `a` and `b`, as `assertEqual` compares
/// them: objects as dicts (a repeated key collapsed to its last value, member order
/// ignored), arrays item by item, strings by their code points, and numbers and booleans by
/// value (`1 == 1.0 == true`, `1.5 == 1.50`; NaN equals nothing). `null` equals only `null`.
/// The derived `==` on `Json` is stricter: it compares members in order and numbers as text.
pub fn py_eq(a: &Json, b: &Json) -> bool {
    if let (Some(x), Some(y)) = (py_num(a), py_num(b)) {
        return num_eq(&x, &y);
    }
    match (a, b) {
        (Json::Null, Json::Null) => true,
        (Json::Str(x), Json::Str(y)) => x == y,
        (Json::Arr(x), Json::Arr(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
        }
        (Json::Obj(x), Json::Obj(y)) => {
            let (x, y) = (dict(x), dict(y));
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| k == k2 && py_eq(v, v2)))
        }
        _ => false,
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&dumps(self))
    }
}

impl fmt::Debug for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Members exactly, duplicates included (Display collapses them).
            Json::Obj(members) => f
                .debug_map()
                .entries(members.iter().map(|(k, v)| (Json::Str(k.clone()), v)))
                .finish(),
            Json::Arr(items) => f.debug_list().entries(items).finish(),
            other => f.write_str(&dumps(other)),
        }
    }
}

// --------------------------------------------------------------------------
// Reading values

impl Json {
    /// An object built from (key, value) pairs, in order.
    pub fn object<'k>(pairs: impl IntoIterator<Item = (&'k str, Json)>) -> Json {
        Json::Obj(
            pairs
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect(),
        )
    }

    /// The value of member `key` (the last one, as Python's dict keeps); None when there is
    /// none or this is not an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members
                .iter()
                .rev()
                .find(|(k, _)| k == key.as_bytes())
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// An object's keys, in order, once each (invalid UTF-8 replaced); empty for anything else.
    pub fn keys(&self) -> Vec<String> {
        match self {
            Json::Obj(members) => dict(members)
                .into_iter()
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A string's WTF-8.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// A string that is valid UTF-8 (one with a lone surrogate is not).
    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|s| std::str::from_utf8(s).ok())
    }

    /// A string as OS bytes (`to_os`), to compare with a real file name.
    pub fn as_os(&self) -> Option<OsString> {
        self.as_bytes().and_then(to_os)
    }

    /// A number written as an integer, when it fits.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(text) => text.parse().ok(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(text) => text.parse().ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(text) => text.parse().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(Vec<u8>, Json)]> {
        match self {
            Json::Obj(members) => Some(members),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
}

impl Index<&str> for Json {
    type Output = Json;

    #[track_caller]
    fn index(&self, key: &str) -> &Json {
        self.get(key)
            .unwrap_or_else(|| panic!("no member {key:?} in {self}"))
    }
}

impl Index<usize> for Json {
    type Output = Json;

    #[track_caller]
    fn index(&self, n: usize) -> &Json {
        self.as_array()
            .and_then(|items| items.get(n))
            .unwrap_or_else(|| panic!("no item {n} in {self}"))
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.as_bytes().to_vec())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s.into_bytes())
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}

impl From<i64> for Json {
    fn from(n: i64) -> Json {
        Json::Num(n.to_string())
    }
}

impl From<i32> for Json {
    fn from(n: i32) -> Json {
        Json::Num(n.to_string())
    }
}

impl From<usize> for Json {
    fn from(n: usize) -> Json {
        Json::Num(n.to_string())
    }
}

/// A Python float: `json.dumps` writes it as its `repr` (`20.0`, `12.5`).
impl From<f64> for Json {
    fn from(f: f64) -> Json {
        Json::Num(json_float(f))
    }
}

impl<T: AsRef<str>> From<&[T]> for Json {
    fn from(items: &[T]) -> Json {
        Json::Arr(items.iter().map(|s| Json::from(s.as_ref())).collect())
    }
}

impl From<Vec<Json>> for Json {
    fn from(items: Vec<Json>) -> Json {
        Json::Arr(items)
    }
}

impl PartialEq<str> for Json {
    fn eq(&self, other: &str) -> bool {
        self.as_bytes() == Some(other.as_bytes())
    }
}

impl PartialEq<&str> for Json {
    fn eq(&self, other: &&str) -> bool {
        self.as_bytes() == Some(other.as_bytes())
    }
}

impl PartialEq<i64> for Json {
    fn eq(&self, other: &i64) -> bool {
        self.as_i64() == Some(*other)
    }
}
