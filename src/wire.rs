//! The JSON wire representation both implementations share (spec, contract item 13;
//! Global Constraints, "The wire representation").
//!
//! Python carries an undecodable path byte `0xXX` as the lone surrogate U+DCXX
//! (`surrogateescape`) and writes JSON with `ensure_ascii=True`, so the byte appears in every
//! file as `\udcXX`. `serde_json` refuses such a string as a `String` or a `Value`, but its
//! `deserialize_bytes` returns any JSON string as WTF-8 (UTF-8 that also allows surrogate code
//! points in their 3-byte form), and keeps distinct JSON strings distinct. `PyStr` holds those
//! bytes exactly, and writes them back as the ASCII literal Python writes, through
//! `RawValue`. So carried text (paths, refs, worker and vendor text) never passes through
//! `Value` or `String`.
//!
//! Surrogateescape happens only at the filesystem boundary: `PyStr::from_os` and
//! `PyStr::to_os`. A JSON object keyed by carried text is a `PyMap`, since `serde_json` takes
//! no `PyStr` as a map key.
//!
//! The writers here (`to_line`, `to_output`, `to_pretty`) escape every string as
//! `ensure_ascii=True` does, so a record Rust writes is ASCII like one Python writes, and write
//! floats as Python's `repr` does.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use serde::de::{self, Unexpected, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::ser::{Formatter, PrettyFormatter};
use serde_json::value::RawValue;

use crate::error::{Error, Result};

/// A record kept exactly as read: never re-serialized, so it is reproduced byte for byte.
pub type RawLine = Box<RawValue>;

/// A JSON string as Python holds it: its WTF-8 bytes, exactly as `deserialize_bytes` returns
/// them. Lone surrogates included, every JSON string round-trips distinctly.
///
/// Invariant: the bytes are well-formed WTF-8 (generalized UTF-8 with no surrogate pair
/// written as two 3-byte halves), which every constructor checks or guarantees.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PyStr(Vec<u8>);

impl PyStr {
    /// The WTF-8 bytes.
    pub fn as_wtf8(&self) -> &[u8] {
        &self.0
    }

    /// The text whose WTF-8 bytes are `bytes`; `None` unless they are well-formed WTF-8.
    pub fn from_wtf8(bytes: Vec<u8>) -> Option<PyStr> {
        is_wtf8(&bytes).then_some(PyStr(bytes))
    }

    /// OS bytes decoded as Python's `surrogateescape` decodes them: each byte that is not part
    /// of valid UTF-8 becomes U+DC80..U+DCFF.
    pub fn from_os(s: &OsStr) -> PyStr {
        let mut out = Vec::with_capacity(s.len());
        for chunk in s.as_bytes().utf8_chunks() {
            out.extend_from_slice(chunk.valid().as_bytes());
            for &byte in chunk.invalid() {
                // Only bytes 0x80..=0xFF are ever invalid: ASCII always decodes.
                push_wtf8(0xDC00 + u32::from(byte), &mut out);
            }
        }
        PyStr(out)
    }

    /// Encoded back as Python's `surrogateescape` encodes: U+DC80..U+DCFF become their bytes.
    /// Any other lone surrogate is an error, as Python's encoder raises.
    pub fn to_os(&self) -> Result<OsString> {
        let mut out = Vec::with_capacity(self.0.len());
        for (position, cp) in code_points(&self.0).enumerate() {
            match cp {
                0xDC80..=0xDCFF => out.push((cp - 0xDC00) as u8),
                0xD800..=0xDFFF => {
                    return Err(Error::msg(format!(
                        "'utf-8' codec can't encode character '\\u{cp:04x}' in position \
                         {position}: surrogates not allowed"
                    )));
                }
                _ => push_wtf8(cp, &mut out),
            }
        }
        Ok(OsString::from_vec(out))
    }

    /// The OS bytes of `parts`, concatenated, decoded as `from_os` decodes them: text made of
    /// generated words and paths, kept lossless.
    pub fn from_os_parts(parts: &[&OsStr]) -> PyStr {
        let mut bytes = Vec::new();
        for part in parts {
            bytes.extend_from_slice(part.as_bytes());
        }
        PyStr::from_os(OsStr::from_bytes(&bytes))
    }

    /// For display: the OS bytes `to_os` gives, shown lossily, as `Path::display` shows a
    /// path (a surrogate `to_os` refuses becomes U+FFFD).
    pub fn shown(&self) -> String {
        match self.to_os() {
            Ok(os) => os.to_string_lossy().into_owned(),
            Err(_) => self.lossy().into_owned(),
        }
    }

    /// For display only: a lone surrogate becomes U+FFFD.
    pub fn lossy(&self) -> Cow<'_, str> {
        match std::str::from_utf8(&self.0) {
            Ok(text) => Cow::Borrowed(text),
            Err(_) => Cow::Owned(
                code_points(&self.0)
                    .map(|cp| char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect(),
            ),
        }
    }
}

impl From<&str> for PyStr {
    /// Valid text, as it is.
    fn from(s: &str) -> PyStr {
        PyStr(s.as_bytes().to_vec())
    }
}

impl From<String> for PyStr {
    /// Valid text, as it is.
    fn from(s: String) -> PyStr {
        PyStr(s.into_bytes())
    }
}

impl PyStr {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Appends `s` as Python's `+` joins strings: a leading surrogate at the end and a
    /// trailing one at the start of `s` become the one character they encode, so the bytes
    /// stay well-formed WTF-8 (and the result is written as the same `\uXXXX` pair).
    pub fn push(&mut self, s: &PyStr) {
        let lead = self
            .0
            .len()
            .checked_sub(3)
            .and_then(|at| decode(&self.0[at..]))
            .map(|(cp, _)| cp)
            .filter(|cp| (0xD800..=0xDBFF).contains(cp));
        let trail = decode(&s.0)
            .map(|(cp, _)| cp)
            .filter(|cp| (0xDC00..=0xDFFF).contains(cp));
        match (lead, trail) {
            (Some(lead), Some(trail)) => {
                self.0.truncate(self.0.len() - 3);
                push_wtf8(
                    0x1_0000 + ((lead - 0xD800) << 10) + (trail - 0xDC00),
                    &mut self.0,
                );
                self.0.extend_from_slice(&s.0[3..]);
            }
            _ => self.0.extend_from_slice(&s.0),
        }
    }

    /// Appends valid text.
    pub fn push_str(&mut self, s: &str) {
        self.0.extend_from_slice(s.as_bytes());
    }

    /// `parts`, joined as `push` joins two.
    pub fn concat(parts: &[&PyStr]) -> PyStr {
        let mut out = PyStr::default();
        for part in parts {
            out.push(part);
        }
        out
    }

    /// `sep.join(parts)`.
    pub fn join(parts: &[PyStr], sep: &str) -> PyStr {
        let mut out = PyStr::default();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                out.push_str(sep);
            }
            out.push(part);
        }
        out
    }

    /// The text's code points, a surrogate escape as its lone surrogate (`for c in s`).
    pub fn code_points(&self) -> impl Iterator<Item = u32> + Clone + '_ {
        code_points(&self.0)
    }

    /// Python's `str.startswith` for valid text.
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.0.starts_with(prefix.as_bytes())
    }

    /// Python's `in` for valid text: WTF-8 is self-synchronizing, so a byte match is a match
    /// of whole characters.
    pub fn contains(&self, needle: &str) -> bool {
        needle.is_empty() || self.0.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    /// `str.strip(chars)` for ASCII `chars` (`str.rstrip` with `start` false). Only ASCII
    /// bytes are cut, and they are never part of a longer character, so the rest is whole.
    pub fn strip_ascii(&self, chars: &[u8], start: bool) -> PyStr {
        let keep = |b: &u8| !chars.contains(b);
        let from = if start {
            self.0.iter().position(keep).unwrap_or(self.0.len())
        } else {
            0
        };
        let to = self
            .0
            .iter()
            .rposition(keep)
            .map_or(from, |i| i + 1)
            .max(from);
        PyStr(self.0[from..to].to_vec())
    }

    /// `repr(s)`, as Python shows it in an error message: a surrogate escape as `\udcXX`.
    pub fn repr(&self) -> String {
        crate::pytext::py_repr_code_points(code_points(&self.0))
    }

    /// The text as UTF-8, as Python's strict `str.encode("utf-8")` gives it: an error, with
    /// Python's `UnicodeEncodeError` text, when it holds any lone surrogate (a surrogate
    /// escape included).
    pub fn encode_utf8(&self) -> Result<&str> {
        if let Ok(text) = std::str::from_utf8(&self.0) {
            return Ok(text);
        }
        // The first run of surrogates, by code point position, as the encoder reports it.
        let surrogate = |cp: &u32| (0xD800..=0xDFFF).contains(cp);
        let cps: Vec<u32> = code_points(&self.0).collect();
        let start = cps.iter().position(surrogate).unwrap_or(0);
        let run = cps[start..].iter().take_while(|cp| surrogate(cp)).count();
        let what = if run == 1 {
            format!("character '\\u{:04x}' in position {start}", cps[start])
        } else {
            format!("characters in position {start}-{}", start + run - 1)
        };
        Err(Error::msg(format!(
            "'utf-8' codec can't encode {what}: surrogates not allowed"
        )))
    }

    /// The OS bytes `to_os` gives, or the lossy text when a surrogate has no byte (for text
    /// printed to a terminal or written to a file, as Python's `surrogateescape` writes it).
    pub fn to_os_lossy(&self) -> OsString {
        self.to_os()
            .unwrap_or_else(|_| OsString::from(self.lossy().into_owned()))
    }
}

impl fmt::Debug for PyStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&ascii_json(self))
    }
}

impl Serialize for PyStr {
    /// The ASCII literal, emitted through `RawValue` so no serializer re-escapes it.
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let raw = RawValue::from_string(ascii_json(self)).map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PyStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<PyStr, D::Error> {
        deserializer.deserialize_bytes(PyStrVisitor)
    }
}

struct PyStrVisitor;

impl Visitor<'_> for PyStrVisitor {
    type Value = PyStr;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<PyStr, E> {
        Ok(PyStr::from(v))
    }

    /// `serde_json` gives every JSON string here, as WTF-8.
    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> std::result::Result<PyStr, E> {
        if is_wtf8(v) {
            Ok(PyStr(v.to_vec()))
        } else {
            Err(E::invalid_value(Unexpected::Bytes(v), &self))
        }
    }
}

/// The code point at the start of `b` and its length, in generalized UTF-8 (surrogates
/// allowed in their 3-byte form); `None` when malformed.
fn decode(b: &[u8]) -> Option<(u32, usize)> {
    let lead = *b.first()?;
    let (len, min, init) = match lead {
        0x00..=0x7F => return Some((u32::from(lead), 1)),
        0xC2..=0xDF => (2, 0x80, u32::from(lead & 0x1F)),
        0xE0..=0xEF => (3, 0x800, u32::from(lead & 0x0F)),
        0xF0..=0xF4 => (4, 0x1_0000, u32::from(lead & 0x07)),
        _ => return None,
    };
    let mut cp = init;
    for &byte in b.get(1..len)? {
        if byte & 0xC0 != 0x80 {
            return None;
        }
        cp = (cp << 6) | u32::from(byte & 0x3F);
    }
    (min..=0x10_FFFF).contains(&cp).then_some((cp, len))
}

/// Whether `b` is well-formed WTF-8: generalized UTF-8 in which no leading surrogate is
/// directly followed by a trailing one (that pair is one 4-byte character).
fn is_wtf8(b: &[u8]) -> bool {
    let (mut i, mut after_lead) = (0, false);
    while i < b.len() {
        let Some((cp, len)) = decode(&b[i..]) else {
            return false;
        };
        if after_lead && (0xDC00..=0xDFFF).contains(&cp) {
            return false;
        }
        after_lead = (0xD800..=0xDBFF).contains(&cp);
        i += len;
    }
    true
}

/// The code points of a `PyStr`'s bytes. They are well-formed by its invariant; a malformed
/// byte would come out as U+FFFD.
fn code_points(b: &[u8]) -> impl Iterator<Item = u32> + Clone + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        let rest = b.get(i..).filter(|r| !r.is_empty())?;
        let (cp, len) = decode(rest).unwrap_or((0xFFFD, 1));
        i += len;
        Some(cp)
    })
}

/// Appends `cp`'s generalized UTF-8 form (a surrogate in 3 bytes).
fn push_wtf8(cp: u32, out: &mut Vec<u8>) {
    // The casts keep the low bits, which the masks have already chosen.
    match cp {
        0..=0x7F => out.push(cp as u8),
        0x80..=0x7FF => out.extend([0xC0 | (cp >> 6) as u8, 0x80 | (cp & 0x3F) as u8]),
        0x800..=0xFFFF => out.extend([
            0xE0 | (cp >> 12) as u8,
            0x80 | ((cp >> 6) & 0x3F) as u8,
            0x80 | (cp & 0x3F) as u8,
        ]),
        _ => out.extend([
            0xF0 | (cp >> 18) as u8,
            0x80 | ((cp >> 12) & 0x3F) as u8,
            0x80 | ((cp >> 6) & 0x3F) as u8,
            0x80 | (cp & 0x3F) as u8,
        ]),
    }
}

/// Appends `cp` as `ensure_ascii=True` writes it inside a string: printable ASCII as is
/// (`"` and `\` escaped), the short escapes for `\n \r \t \b \f`, and `\uXXXX` for every
/// other UTF-16 code unit (a surrogate pair above U+FFFF).
fn push_escaped(cp: u32, out: &mut String) {
    match cp {
        0x22 => out.push_str("\\\""),
        0x5C => out.push_str("\\\\"),
        0x0A => out.push_str("\\n"),
        0x0D => out.push_str("\\r"),
        0x09 => out.push_str("\\t"),
        0x08 => out.push_str("\\b"),
        0x0C => out.push_str("\\f"),
        0x20..=0x7E => out.push(char::from(cp as u8)),
        0x1_0000.. => {
            let v = cp - 0x1_0000;
            out.push_str(&format!(
                "\\u{:04x}\\u{:04x}",
                0xD800 + (v >> 10),
                0xDC00 + (v & 0x3FF)
            ));
        }
        _ => out.push_str(&format!("\\u{cp:04x}")),
    }
}

/// The quoted JSON literal Python's `ensure_ascii=True` writes for `s`.
pub fn ascii_json(s: &PyStr) -> String {
    let mut out = String::with_capacity(s.0.len() + 2);
    out.push('"');
    for cp in code_points(&s.0) {
        push_escaped(cp, &mut out);
    }
    out.push('"');
    out
}

/// A JSON object whose keys are carried text (ref names, file names, model names): its
/// entries in file order, as a Python `dict` keeps them. `serde_json` takes no `PyStr` as a
/// map key, so this is the map type for any record keyed by such text.
///
/// A repeated key keeps its first position and its last value, as `json.loads` does. Written
/// as a JSON object; when a key is not valid UTF-8 (a lone surrogate), the object is written
/// compact (`to_line`'s layout) whatever the writer's layout, the same JSON either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PyMap<V>(pub Vec<(PyStr, V)>);

impl<V> Default for PyMap<V> {
    fn default() -> PyMap<V> {
        PyMap(Vec::new())
    }
}

impl<V> PyMap<V> {
    pub fn get(&self, key: &PyStr) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.0.iter().map(|(_, v)| v)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The value at `key`, inserted last (as `dict.setdefault`) when it is not there.
    pub fn entry(&mut self, key: &PyStr, default: impl FnOnce() -> V) -> &mut V {
        let at = match self.0.iter().position(|(k, _)| k == key) {
            Some(at) => at,
            None => {
                self.0.push((key.clone(), default()));
                self.0.len() - 1
            }
        };
        &mut self.0[at].1
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for PyMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct MapVisitor<V>(std::marker::PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for MapVisitor<V> {
            type Value = PyMap<V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> std::result::Result<PyMap<V>, A::Error> {
                let mut out: Vec<(PyStr, V)> = Vec::new();
                while let Some(key) = access.next_key::<PyStr>()? {
                    let value = access.next_value::<V>()?;
                    match out.iter_mut().find(|(k, _)| *k == key) {
                        Some(entry) => entry.1 = value,
                        None => out.push((key, value)),
                    }
                }
                Ok(PyMap(out))
            }
        }

        deserializer.deserialize_map(MapVisitor(std::marker::PhantomData))
    }
}

impl<V: Serialize> Serialize for PyMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{Error as _, SerializeMap};
        let keys: Option<Vec<&str>> = self
            .0
            .iter()
            .map(|(k, _)| std::str::from_utf8(&k.0).ok())
            .collect();
        if let Some(keys) = keys {
            let mut map = serializer.serialize_map(Some(self.0.len()))?;
            for (key, (_, value)) in keys.into_iter().zip(&self.0) {
                map.serialize_entry(key, value)?;
            }
            return map.end();
        }
        let mut text = String::from("{");
        for (i, (key, value)) in self.0.iter().enumerate() {
            if i > 0 {
                text.push(',');
            }
            text.push_str(&ascii_json(key));
            text.push(':');
            text.push_str(&to_line(value).map_err(|e| S::Error::custom(e.message))?);
        }
        text.push('}');
        RawValue::from_string(text)
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}

/// For an `Option<Option<T>>` field read with `#[serde(default, deserialize_with =
/// "wire::present")]`: an absent field is `None`, `null` is `Some(None)`. For the fields whose
/// absence and `null` the Python tells apart (`d.get(key, default)`, `key in d`).
pub fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// The three non-finite floats, as the `i128` values `allow_nan` hands the serializer in their
/// place: `serde_json` writes any non-finite `f64` as `null` before a formatter sees it, so
/// they reach `PyFormatter::write_i128` as these, which no record holds as an integer.
const NAN_MARK: i128 = i128::MIN;
const INFINITY_MARK: i128 = i128::MIN + 1;
const NEG_INFINITY_MARK: i128 = i128::MIN + 2;

/// For an `f64` field written with `#[serde(serialize_with = "wire::allow_nan")]`: a finite
/// value as any float, and `NaN`, `Infinity` and `-Infinity` as Python's `json.dump` writes
/// them (`allow_nan=True`), which only the writers here can do.
pub fn allow_nan<S: Serializer>(x: &f64, s: S) -> std::result::Result<S::Ok, S::Error> {
    match *x {
        x if x.is_nan() => s.serialize_i128(NAN_MARK),
        f64::INFINITY => s.serialize_i128(INFINITY_MARK),
        f64::NEG_INFINITY => s.serialize_i128(NEG_INFINITY_MARK),
        x => s.serialize_f64(x),
    }
}

/// The record's `kind`, read alone; no other field is validated.
pub fn event_kind(line: &RawLine) -> Result<String> {
    #[derive(Deserialize)]
    struct Kind {
        kind: String,
    }
    serde_json::from_str::<Kind>(line.get())
        .map(|k| k.kind)
        .map_err(|e| Error::msg(format!("a record with no readable kind: {e}")))
}

/// One JSONL line, without its newline: `json.dumps(v, ensure_ascii=True,
/// separators=(",", ":"))`.
pub fn to_line<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    write(value, Layout::Line)
}

/// What a command prints: `json.dumps(v, ensure_ascii=True)`.
pub fn to_output<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    write(value, Layout::Spaced)
}

/// A JSON file's body, without its final newline: `json.dump(v, f, indent=2,
/// ensure_ascii=True)`.
pub fn to_pretty<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    write(value, Layout::Pretty(PrettyFormatter::with_indent(b"  ")))
}

fn write<T: Serialize + ?Sized>(value: &T, layout: Layout) -> Result<String> {
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, PyFormatter(layout));
    value
        .serialize(&mut serializer)
        .map_err(|e| Error::msg(format!("cannot write the record as JSON: {e}")))?;
    // ASCII, apart from any raw value kept as read, which is a `str`.
    Ok(String::from_utf8(out).expect("serde_json writes UTF-8"))
}

/// Python's three `json.dumps` layouts.
enum Layout {
    /// `separators=(",", ":")`.
    Line,
    /// The default separators, `", "` and `": "`.
    Spaced,
    /// `indent=2`.
    Pretty(PrettyFormatter<'static>),
}

/// `serde_json`'s writer with Python's layouts and `ensure_ascii=True` escaping.
struct PyFormatter(Layout);

impl Formatter for PyFormatter {
    fn begin_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.begin_array(w),
            _ => w.write_all(b"["),
        }
    }

    fn end_array<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.end_array(w),
            _ => w.write_all(b"]"),
        }
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.begin_array_value(w, first),
            Layout::Spaced if !first => w.write_all(b", "),
            Layout::Line if !first => w.write_all(b","),
            _ => Ok(()),
        }
    }

    fn end_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.end_array_value(w),
            _ => Ok(()),
        }
    }

    fn begin_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.begin_object(w),
            _ => w.write_all(b"{"),
        }
    }

    fn end_object<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.end_object(w),
            _ => w.write_all(b"}"),
        }
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.begin_object_key(w, first),
            Layout::Spaced if !first => w.write_all(b", "),
            Layout::Line if !first => w.write_all(b","),
            _ => Ok(()),
        }
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.begin_object_value(w),
            Layout::Spaced => w.write_all(b": "),
            Layout::Line => w.write_all(b":"),
        }
    }

    fn end_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        match &mut self.0 {
            Layout::Pretty(p) => p.end_object_value(w),
            _ => Ok(()),
        }
    }

    /// A float as Python's `repr` writes it (`1e-05`, not `1e-5`). `serde_json` writes a
    /// non-finite float as `null` without calling this.
    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        w.write_all(crate::pytext::py_float(value).as_bytes())
    }

    /// An integer, or one of the non-finite floats `allow_nan` marks, as Python writes it.
    fn write_i128<W: ?Sized + io::Write>(&mut self, w: &mut W, value: i128) -> io::Result<()> {
        let text = match value {
            NAN_MARK => "NaN".to_string(),
            INFINITY_MARK => "Infinity".to_string(),
            NEG_INFINITY_MARK => "-Infinity".to_string(),
            n => n.to_string(),
        };
        w.write_all(text.as_bytes())
    }

    /// The unescaped run of a string: everything but `"`, `\` and control characters, which
    /// `serde_json` escapes as Python does. What is not printable ASCII is escaped here.
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        if fragment.bytes().all(|b| (0x20..0x7F).contains(&b)) {
            return w.write_all(fragment.as_bytes());
        }
        let mut out = String::with_capacity(fragment.len() * 2);
        for c in fragment.chars() {
            push_escaped(u32::from(c), &mut out);
        }
        w.write_all(out.as_bytes())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    use serde::{Deserialize, Serialize};
    use serde_json::value::RawValue;

    use super::*;
    use crate::store::{append_jsonl, read_jsonl};

    /// A file of `tests/fixtures/reference/`: what the reference wrote, recorded by the generator
    /// there and read at run time.
    pub(crate) fn reference_fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/reference")
            .join(name)
    }

    /// `bytes` in lowercase hex, as the reference fixtures give bytes.
    pub(crate) fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The test-only binary `name` (`leadhand-fake-codex`, `leadhand-fake-claude`), which a
    /// `--features testing` build puts in the profile folder: the nearest ancestor of this
    /// test binary that holds Cargo's `.cargo-lock` (`target/debug`, or
    /// `<target dir>/<triple>/<profile>` under `CARGO_TARGET_DIR`, `--target-dir` or
    /// `--target`). The search stops there, so it never finds an unrelated file higher up.
    ///
    /// A unit-test run (`--lib`) builds no `[[bin]]` target: it runs the binary an earlier
    /// build left. After editing a fake, run the full `cargo test --features testing` once.
    #[cfg(feature = "testing")]
    pub(crate) fn fake_bin(name: &str) -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap();
        let profile = exe
            .ancestors()
            .skip(1)
            .find(|dir| dir.join(".cargo-lock").is_file())
            .unwrap_or_else(|| panic!("no Cargo profile folder above {}", exe.display()));
        let bin = profile.join(name);
        assert!(
            bin.is_file(),
            "no {name} in {}: build with `cargo test --features testing` (and after editing \
             the fake, run the full `cargo test --features testing` once: a `--lib` run does \
             not rebuild it)",
            profile.display()
        );
        bin
    }

    /// The bytes of the reference fixture `name`, failing the test when it is missing.
    #[track_caller]
    pub(crate) fn read_reference(name: &str) -> Vec<u8> {
        let path = reference_fixture(name);
        fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    }

    /// The reference fixture of the tests here.
    const FIXTURE: &str = "wire.json";

    /// The reference fixture `name` as UTF-8 text.
    #[track_caller]
    pub(crate) fn read_reference_text(name: &str) -> String {
        String::from_utf8(read_reference(name)).unwrap()
    }

    /// The reference fixture `name` as JSON (one whose strings hold no lone surrogate).
    #[track_caller]
    pub(crate) fn reference_json(name: &str) -> serde_json::Value {
        serde_json::from_slice(&read_reference(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// The `out` of each row of `section` in the reference fixture `name` (a JSON object of
    /// arrays of `{"in": .., "out": ..}` rows), once the rows' `in` are checked to be `inputs`,
    /// in order: what the reference made of the inputs the test gives its own code.
    #[track_caller]
    pub(crate) fn reference_outputs(
        name: &str,
        section: &str,
        inputs: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        let all = reference_json(name);
        let rows = all[section]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: no section {section:?}"));
        let recorded: Vec<&serde_json::Value> = rows.iter().map(|r| &r["in"]).collect();
        assert_eq!(
            recorded,
            inputs.iter().collect::<Vec<_>>(),
            "{name} {section}: recorded on other inputs"
        );
        rows.iter().map(|r| r["out"].clone()).collect()
    }

    /// A record line with its `ts` written `<ts>`: a line the reference wrote at its fixed time,
    /// beside one written now.
    pub(crate) fn mask_ts(line: &str) -> String {
        let (head, rest) = line.split_once(",\"ts\":\"").expect("a ts field");
        let (_, tail) = rest.split_once('"').expect("the ts's closing quote");
        format!("{head},\"ts\":\"<ts>\"{tail}")
    }

    /// A float as the reference fixtures give an input float: its bits, in hex.
    pub(crate) fn float_bits(x: f64) -> serde_json::Value {
        serde_json::json!(format!("{:016x}", x.to_bits()))
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Rec {
        path: PyStr,
    }

    /// `python-events.jsonl`: records the reference's `append_jsonl` wrote, with a surrogate
    /// escape (`b'a\xffb'`), non-ASCII text, an astral character, a path of every kind at once
    /// and a lone surrogate. Rust reads each one, and its rewrite of them is the same bytes.
    #[test]
    fn reference_events_with_raw_bytes_round_trip() {
        let written = read_reference("python-events.jsonl");
        let first = written.split_inclusive(|&b| b == b'\n').next().unwrap();
        assert_eq!(first, b"{\"path\":\"a\\udcffb\"}\n");

        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path();
        let file = run.join("records.jsonl");
        fs::write(&file, &written).unwrap();
        let read: Vec<(Rec, RawLine)> = read_jsonl(run, &file).unwrap();
        assert_eq!(read.len(), 5);
        let rec = &read[0].0;
        assert_eq!(rec.path.as_wtf8(), b"\x61\xed\xb3\xbf\x62");
        assert_eq!(rec.path.to_os().unwrap().into_vec(), b"\x61\xff\x62");
        assert_eq!(to_line(rec).unwrap(), r#"{"path":"a\udcffb"}"#);
        assert_eq!(read[1].0.path.as_wtf8(), "caf\u{e9} \u{2615}".as_bytes());
        assert_eq!(read[2].0.path.as_wtf8(), "\u{1f600}/x".as_bytes());
        assert_eq!(
            read[3].0.path.to_os().unwrap().into_vec(),
            b"/ws/caf\xc3\xa9/\xfe\xff/\xf0\x9f\x98\x80"
        );
        assert_eq!(read[4].0.path.as_wtf8(), b"x\xed\xa0\x80y");

        // Written back through the store, line by line, the records are the reference's bytes,
        // and read again they are the same strings.
        let again = run.join("again.jsonl");
        for (rec, _) in &read {
            append_jsonl(run, &again, rec).unwrap();
        }
        assert_eq!(fs::read(&again).unwrap(), written);
        let reread: Vec<(Rec, RawLine)> = read_jsonl(run, &again).unwrap();
        let paths = |recs: &[(Rec, RawLine)]| -> Vec<Vec<u8>> {
            recs.iter()
                .map(|(r, _)| r.path.as_wtf8().to_vec())
                .collect()
        };
        assert_eq!(paths(&reread), paths(&read));
    }

    #[test]
    fn distinct_strings_stay_distinct() {
        let read = |literal: &str| {
            serde_json::from_str::<Rec>(&format!(r#"{{"path":{literal}}}"#))
                .unwrap()
                .path
        };
        let cases: [(&str, &[u8]); 4] = [
            (r#""\u00e9""#, b"\xc3\xa9"),
            (r#""\udcc3\udca9""#, b"\xed\xb3\x83\xed\xb2\xa9"),
            (r#""\ud800""#, b"\xed\xa0\x80"),
            (
                r#""\udced\udca0\udc80""#,
                b"\xed\xb3\xad\xed\xb2\xa0\xed\xb2\x80",
            ),
        ];
        for (literal, wtf8) in cases {
            let s = read(literal);
            assert_eq!(s.as_wtf8(), wtf8, "{literal}");
            assert_eq!(ascii_json(&s), literal);
            assert_eq!(
                to_line(&Rec { path: s }).unwrap(),
                format!(r#"{{"path":{literal}}}"#)
            );
        }
        assert_ne!(read(cases[0].0), read(cases[1].0));
        assert_ne!(read(cases[2].0), read(cases[3].0));
    }

    #[test]
    fn fs_boundary_is_surrogateescape() {
        let e_acute = PyStr::from_os(OsStr::from_bytes(b"\xc3\xa9"));
        assert_eq!(ascii_json(&e_acute), r#""\u00e9""#);
        assert_eq!(e_acute.to_os().unwrap().into_vec(), b"\xc3\xa9");
        let ff = PyStr::from_os(OsStr::from_bytes(b"\xff"));
        assert_eq!(ascii_json(&ff), r#""\udcff""#);
        assert_eq!(ff.to_os().unwrap().into_vec(), b"\xff");

        // Harder byte strings decode as Python's surrogateescape decodes them, and encode back.
        let cases: [&[u8]; 7] = [
            b"\xed\xa0\x80",
            b"\xe2\x82A",
            b"\xf0\x9f\x98A",
            b"a\xc0\xafb",
            b"\xf4\x90\x80\x80",
            b"ok\xf0\x9f\x98\x80\xe9",
            b"\x80\xbf\xc2",
        ];
        // Each case in hex; the reference decoded it and wrote the string as JSON.
        let hex: Vec<serde_json::Value> = cases.iter().map(|b| serde_json::json!(hex(b))).collect();
        let decoded = reference_outputs(FIXTURE, "surrogateescape", &hex);
        for (bytes, want) in cases.iter().zip(decoded) {
            let s = PyStr::from_os(OsStr::from_bytes(bytes));
            assert_eq!(ascii_json(&s), want.as_str().unwrap(), "{bytes:x?}");
            assert_eq!(s.to_os().unwrap().into_vec(), *bytes);
        }
    }

    #[test]
    fn to_os_refuses_other_lone_surrogates() {
        let read = |literal: &str| {
            serde_json::from_str::<Rec>(&format!(r#"{{"path":{literal}}}"#))
                .unwrap()
                .path
        };
        let err = read(r#""a\ud800""#).to_os().unwrap_err();
        assert_eq!(
            err.message,
            r"'utf-8' codec can't encode character '\ud800' in position 1: surrogates not allowed"
        );
        assert!(read(r#""\udc7f""#).to_os().is_err());
        assert!(read(r#""\ude00""#).to_os().is_err());
        assert_eq!(read(r#""\udc80""#).to_os().unwrap().into_vec(), b"\x80");
        assert_eq!(read(r#""\ud83d\ude00""#).lossy(), "\u{1f600}");
        assert_eq!(read(r#""a\ud800b""#).lossy(), "a\u{fffd}b");
    }

    #[test]
    fn lone_surrogate_survives_storage_action_and_output() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path();
        // The event the reference appended: a ruling whose path is `b'a\xffb'`.
        let recorded = &reference_json(FIXTURE)["lone_surrogate"][0];
        fs::write(
            run.join("events.jsonl"),
            recorded["events"].as_str().unwrap(),
        )
        .unwrap();

        // The event, read typed, with its raw line.
        #[derive(Deserialize)]
        struct Ruling {
            seq: u64,
            path: PyStr,
        }
        let events: Vec<(Ruling, RawLine)> = read_jsonl(run, &run.join("events.jsonl")).unwrap();
        let (event, raw) = &events[0];
        assert_eq!(
            (event.seq, event_kind(raw).unwrap().as_str()),
            (1, "ruling")
        );
        assert_eq!(event.path.as_wtf8(), b"a\xed\xb3\xbfb");

        // A stored record.
        #[derive(Serialize, Deserialize)]
        struct Stored {
            card: u32,
            path: PyStr,
        }
        let attempts = run.join("attempts.jsonl");
        let line = append_jsonl(
            run,
            &attempts,
            &Stored {
                card: 1,
                path: event.path.clone(),
            },
        )
        .unwrap();
        assert_eq!(line.get(), r#"{"card":1,"path":"a\udcffb"}"#);
        let stored: Vec<(Stored, RawLine)> = read_jsonl(run, &attempts).unwrap();
        assert_eq!(stored[0].0.path, event.path);

        // An action the CLI prints, carrying the path and the event as found.
        #[derive(Serialize)]
        struct Action<'a> {
            action: &'static str,
            path: &'a PyStr,
            event: &'a RawValue,
        }
        let printed = to_output(&Action {
            action: "dispatch",
            path: &stored[0].0.path,
            event: raw,
        })
        .unwrap();
        assert_eq!(
            printed,
            format!(
                r#"{{"action": "dispatch", "path": "a\udcffb", "event": {}}}"#,
                raw.get()
            )
        );

        // A RawLine round trip leaves it byte-identical.
        let again: RawLine = serde_json::from_str(&printed).unwrap();
        assert_eq!(serde_json::to_string(&again).unwrap(), printed);
        #[derive(Deserialize)]
        struct Printed {
            path: PyStr,
            event: RawLine,
        }
        let back: Printed = serde_json::from_str(again.get()).unwrap();
        assert_eq!(back.path.as_wtf8(), b"a\xed\xb3\xbfb");
        assert_eq!(back.event.get(), raw.get());

        // No `Value` was involved: any step through one would have refused the string.
        assert!(serde_json::from_str::<serde_json::Value>(raw.get()).is_err());
        assert!(serde_json::from_str::<serde_json::Value>(&printed).is_err());

        // And Python reads the printed path as the string it wrote: the reference read this
        // text so.
        assert_eq!(recorded["printed"].as_str().unwrap(), printed);
        assert_eq!(recorded["reads_back"], "True\n");
    }

    #[test]
    fn vendor_reader_ignores_lone_surrogate_in_unused_field() {
        #[derive(Deserialize)]
        struct Message {
            id: String,
        }
        let line = r#"{"id":"m1","text":"x\udcffy"}"#;
        assert_eq!(serde_json::from_str::<Message>(line).unwrap().id, "m1");
        // A reader through `Value` would have failed on the field it does not use.
        assert!(serde_json::from_str::<serde_json::Value>(line).is_err());
    }

    #[test]
    fn non_ascii_text_written_as_ascii_escapes() {
        #[derive(Serialize)]
        struct Doc<'a> {
            s: &'a str,
            p: PyStr,
            n: [u8; 2],
            e: [u8; 0],
            o: BTreeMap<String, u8>,
        }
        let strings = [
            "\u{e9}",
            "\u{1f600}",
            "a\"b\\c\n\t\r\u{8}\u{c}\u{1}\u{1f}\u{7f}/",
            "\u{4e2d}\u{6587} \u{2028}",
            "plain",
        ];
        // The reference's dumps of each string, and of `{'s': s, 'p': s, 'n': [1, 2], 'e': [],
        // 'o': {}}` as a line, as output and pretty.
        let inputs: Vec<serde_json::Value> = strings.iter().map(|s| serde_json::json!(s)).collect();
        let dumped = reference_outputs(FIXTURE, "ascii_escapes", &inputs);
        for (s, want) in strings.iter().zip(dumped) {
            let want: Vec<String> = serde_json::from_value(want).unwrap();
            let doc = Doc {
                s,
                p: PyStr::from(*s),
                n: [1, 2],
                e: [],
                o: BTreeMap::new(),
            };
            assert_eq!(ascii_json(&doc.p), want[0], "{s:?}");
            assert_eq!(to_line(&doc).unwrap(), want[1], "{s:?}");
            assert_eq!(to_output(&doc).unwrap(), want[2], "{s:?}");
            assert_eq!(to_pretty(&doc).unwrap(), want[3], "{s:?}");
        }
        assert_eq!(ascii_json(&PyStr::from("\u{e9}")), r#""\u00e9""#);
        assert_eq!(ascii_json(&PyStr::from("\u{1f600}")), r#""\ud83d\ude00""#);
    }

    #[test]
    fn event_kind_reads_only_kind() {
        let raw: RawLine =
            serde_json::from_str(r#"{"seq":3,"path":"\udcff","kind":"ruling"}"#).unwrap();
        assert_eq!(event_kind(&raw).unwrap(), "ruling");
        let none: RawLine = serde_json::from_str(r#"{"seq":3}"#).unwrap();
        assert!(event_kind(&none).is_err());
    }

    #[test]
    fn pymap_keeps_order_and_carried_keys() {
        // Keys a Python dict holds: a non-UTF-8 ref name, non-ASCII and astral text; a repeated
        // key keeps its first position and its last value, as json.loads does.
        let text = r#"{"refs/heads/a\udcffb": "x", "\u00e9": "y", "\ud83d\ude00": "z", "refs/heads/a\udcffb": "w"}"#;
        let map: PyMap<PyStr> = serde_json::from_str(text).unwrap();
        let keys: Vec<&[u8]> = map.0.iter().map(|(k, _)| k.as_wtf8()).collect();
        assert_eq!(
            keys,
            [
                &b"refs/heads/a\xed\xb3\xbfb"[..],
                b"\xc3\xa9",
                "\u{1f600}".as_bytes()
            ]
        );
        assert_eq!(map.get(&PyStr::from("\u{e9}")), Some(&PyStr::from("y")));
        let want = r#"{"refs/heads/a\udcffb":"w","\u00e9":"y","\ud83d\ude00":"z"}"#;
        assert_eq!(to_line(&map).unwrap(), want);
        // Python reads it back as the same dict: the reference read this text with the keys
        // `['refs/heads/a\udcffb', 'é', '\U0001f600']`, in order, and `'é'` -> `y`.
        let read_back = reference_outputs(FIXTURE, "pymap_read_back", &[serde_json::json!(want)]);
        assert_eq!(read_back, ["True y\n"]);
        // With UTF-8 keys every layout is Python's own.
        let plain: PyMap<u32> = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
        assert_eq!(to_output(&plain).unwrap(), r#"{"b": 1, "a": 2}"#);
        assert_eq!(to_pretty(&plain).unwrap(), "{\n  \"b\": 1,\n  \"a\": 2\n}");
    }

    #[test]
    fn push_joins_text_as_python_concatenates() {
        let read = |literal: &str| serde_json::from_str::<PyStr>(literal).unwrap();
        // A leading surrogate then a trailing one: the character they encode, written as the
        // same pair Python writes for the joined string.
        let mut s = read(r#""a\ud83d""#);
        s.push(&read(r#""\ude00b""#));
        assert_eq!(s.as_wtf8(), "a\u{1f600}b".as_bytes());
        assert_eq!(ascii_json(&s), r#""a\ud83d\ude00b""#);
        // Anything else is kept as it is.
        let mut t = read(r#""\udcff""#);
        t.push_str(" and ");
        t.push(&read(r#""\ud800""#));
        assert_eq!(ascii_json(&t), r#""\udcff and \ud800""#);
        assert!(t.contains(" and ") && !t.starts_with(" "));
        assert_eq!(
            PyStr::concat(&[&PyStr::from("x"), &read(r#""\udc80""#)]).as_wtf8(),
            b"x\xed\xb2\x80"
        );
    }

    #[test]
    fn floats_are_written_as_python_writes_them() {
        let floats = vec![
            0.0, 1.0, 15.0, 0.1, 5.2e-05, 1e-07, 1e16, 123456.789, -2.5, 1e22,
        ];
        // The reference's line of the same floats (given by their bits).
        let bits: Vec<serde_json::Value> = floats.iter().map(|x| float_bits(*x)).collect();
        let want = reference_outputs(FIXTURE, "floats", &[serde_json::json!(bits)]);
        assert_eq!(
            format!("{}\n", to_line(&floats).unwrap()),
            want[0].as_str().unwrap()
        );
    }

    #[test]
    fn invalid_utf8_bytes_are_not_a_pystr() {
        // Only a reader that hands over raw bytes could produce these; the store reads text.
        assert!(serde_json::from_slice::<Rec>(b"{\"path\":\"a\xffb\"}").is_err());
    }

    #[test]
    fn repr_matches_python_with_lone_surrogates() {
        let literals = [
            r#""N\udcff""#,
            r#""it's \ud800""#,
            r#""\"q\" and 'q' \udfff\n""#,
            r#""café 😀 \u0007""#,
            r#""""#,
        ];
        let texts: Vec<PyStr> = literals
            .iter()
            .map(|t| serde_json::from_str(t).unwrap())
            .collect();
        // The reference's `repr` of each string, read from its JSON literal.
        let inputs: Vec<serde_json::Value> =
            literals.iter().map(|l| serde_json::json!(l)).collect();
        let theirs: Vec<String> = reference_outputs(FIXTURE, "repr", &inputs)
            .into_iter()
            .map(|v| serde_json::from_value(v).unwrap())
            .collect();
        let ours: Vec<String> = texts.iter().map(PyStr::repr).collect();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn encode_utf8_is_python_strict_encode() {
        let literals = [
            r#""plain café 😀""#,
            r#""ab\udcff\udcfecd\udcffe""#,
            r#""x\udcffy""#,
            r#""é😀\udcff""#,
            r#""lone \ud800 lead""#,
            r#""\udfff""#,
            r#""""#,
        ];
        let texts: Vec<PyStr> = literals
            .iter()
            .map(|t| serde_json::from_str(t).unwrap())
            .collect();
        // The reference's strict encode of each string, read from its JSON literal: `ok` and
        // the text, or `err` and the error's message.
        let inputs: Vec<serde_json::Value> =
            literals.iter().map(|l| serde_json::json!(l)).collect();
        let theirs: Vec<(String, String)> = reference_outputs(FIXTURE, "encode_utf8", &inputs)
            .into_iter()
            .map(|v| serde_json::from_value(v).unwrap())
            .collect();
        assert_eq!(theirs.len(), texts.len());
        for (text, (kind, want)) in texts.iter().zip(&theirs) {
            let ours = match text.encode_utf8() {
                Ok(s) => ("ok", s.to_string()),
                Err(e) => ("err", e.message),
            };
            assert_eq!((ours.0, &ours.1), (kind.as_str(), want), "{text:?}");
        }
    }
}
