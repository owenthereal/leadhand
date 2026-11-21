//! The Codex meter (`status.codex_meter`): the latest rate-limit reading in the newest Codex
//! rollout files.
//!
//! Rollouts are vendor files, so only the fields the meter uses are read, and leniently, as
//! the Python's `dict` lookups and `isinstance` checks read them: a field of another JSON type
//! is no value, never an error. `Lenient` is that decoding, shared by the rollout readers.

use std::marker::PhantomData;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::pytext::{py_float, py_splitlines};
use crate::wire::PyStr;

/// The newest rollout files the Codex meter reads.
pub const METER_SCAN_FILES: usize = 20;

/// A Codex rate-limit reading: the used percent and when it was observed (`null` both when
/// there is none).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meter {
    pub percent: Option<f64>,
    pub observed_at: Option<PyStr>,
}

/// The latest rate-limit record in the newest Codex rollout files under `sessions_dir`,
/// preferring the weekly window (`status.codex_meter`); none when there is none.
pub fn codex_meter(sessions_dir: &Path) -> Meter {
    let mut files = Vec::new();
    rollouts(sessions_dir, &mut files);
    let mut dated = Vec::with_capacity(files.len());
    for file in files {
        // A matched name that cannot be stat'ed (a dangling link) ends the scan, as the
        // Python's sort raises.
        let Ok(meta) = std::fs::metadata(&file) else {
            return Meter::default();
        };
        dated.push((meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9, file));
    }
    // Newest first; equal times keep their order, as Python's stable sort does.
    dated.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut best: Option<(PyStr, f64)> = None;
    for (_, file) in dated.iter().take(METER_SCAN_FILES) {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in py_splitlines(&text) {
            if !line.contains("rate_limits") {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<RateRecord>(line) else {
                continue;
            };
            let limits = rec.payload.rate_limits;
            let windows: Vec<&Window> = [&limits.primary, &limits.secondary]
                .into_iter()
                .filter(|w| w.used_percent.is_some())
                .collect();
            let weekly = windows.iter().find(|w| w.window_minutes == Some(10080.0));
            let Some(window) = weekly.or(windows.first()) else {
                continue;
            };
            let stamp = rec.timestamp.0;
            if best.as_ref().is_none_or(|(at, _)| stamp >= *at) {
                best = Some((stamp, window.used_percent.unwrap_or_default()));
            }
        }
        if best.is_some() {
            break;
        }
    }
    match best {
        Some((stamp, percent)) => Meter {
            percent: Some(percent),
            observed_at: Some(stamp).filter(|s| !s.is_empty()),
        },
        None => Meter::default(),
    }
}

/// Every entry under `dir`, at any depth, named `rollout-*.jsonl` (`Path.glob`, which
/// matches hidden names and does not follow linked directories).
fn rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let bytes = name.as_bytes();
        if bytes.len() >= "rollout-.jsonl".len()
            && bytes.starts_with(b"rollout-")
            && bytes.ends_with(b".jsonl")
        {
            out.push(entry.path());
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            subdirs.push(entry.path());
        }
    }
    for sub in subdirs {
        rollouts(&sub, out);
    }
}

// --------------------------------------------------------------------------
// Lenient decoding

/// A value read from any JSON value: each kind it reads overrides its method; every other
/// kind (and any array, and an object unless `object` is overridden) is consumed unread and
/// gives `Default`.
pub trait Lenient: Default {
    fn boolean(_: bool) -> Self {
        Self::default()
    }
    fn int(_: i64) -> Self {
        Self::default()
    }
    fn uint(_: u64) -> Self {
        Self::default()
    }
    fn float(_: f64) -> Self {
        Self::default()
    }
    fn text(_: &str) -> Self {
        Self::default()
    }
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Self::default())
    }
}

/// Reads a `Lenient` value from whatever JSON value is there.
pub fn lenient<'de, D: Deserializer<'de>, T: Lenient>(d: D) -> Result<T, D::Error> {
    struct V<T>(PhantomData<T>);

    impl<'de, T: Lenient> Visitor<'de> for V<T> {
        type Value = T;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("any JSON value")
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<T, E> {
            Ok(T::boolean(v))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<T, E> {
            Ok(T::int(v))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<T, E> {
            Ok(T::uint(v))
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<T, E> {
            Ok(T::float(v))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<T, E> {
            Ok(T::text(v))
        }
        fn visit_unit<E: de::Error>(self) -> Result<T, E> {
            Ok(T::default())
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::object(map)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<T, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(T::default())
        }
    }

    d.deserialize_any(V(PhantomData))
}

/// `Deserialize` through `Lenient`.
macro_rules! deserialize_leniently {
    ($($t:ty),*) => {$(
        impl<'de> serde::Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(
                d: D,
            ) -> std::result::Result<Self, D::Error> {
                $crate::usage::meter::lenient(d)
            }
        }
    )*};
}

pub(crate) use deserialize_leniently;

deserialize_leniently!(Window, Number, Stamp, Count);

/// A token count as `usage._int` reads it: a JSON number (not a `bool`), truncated and
/// floored at 0; any other value is 0. A count beyond `u64` saturates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Count(pub u64);

impl Lenient for Count {
    fn int(v: i64) -> Self {
        Count(u64::try_from(v).unwrap_or(0))
    }
    fn uint(v: u64) -> Self {
        Count(v)
    }
    fn float(v: f64) -> Self {
        // `as` truncates toward zero and saturates; NaN is 0.
        Count(if v > 0.0 { v as u64 } else { 0 })
    }
}

/// A JSON string as `json.loads` gives it, lone surrogates included (the field is read raw,
/// unvalidated, and only a string is decoded), else `None`: a field Python reads with
/// `isinstance(x, str)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct VendorStr(pub Option<PyStr>);

impl<'de> Deserialize<'de> for VendorStr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Box::<serde_json::value::RawValue>::deserialize(d)?;
        if !raw.get().starts_with('"') {
            return Ok(VendorStr(None));
        }
        serde_json::from_str::<PyStr>(raw.get())
            .map(|s| VendorStr(Some(s)))
            .map_err(de::Error::custom)
    }
}

impl VendorStr {
    /// Whether the value is the string `s`.
    pub(crate) fn is(&self, s: &str) -> bool {
        self.0.as_ref().is_some_and(|v| v.as_wtf8() == s.as_bytes())
    }
}

/// A rollout line with a rate-limit record: only the fields the meter reads.
#[derive(Deserialize)]
struct RateRecord {
    payload: RatePayload,
    #[serde(default)]
    timestamp: Stamp,
}

#[derive(Deserialize)]
struct RatePayload {
    rate_limits: RateLimits,
}

#[derive(Deserialize)]
struct RateLimits {
    #[serde(default)]
    primary: Window,
    #[serde(default)]
    secondary: Window,
}

/// A rate-limit window: `used_percent` set only when the window is an object whose
/// `used_percent` is a number (Python's `isinstance(w, dict)` and `(int, float)`, `bool`
/// included).
#[derive(Default)]
struct Window {
    used_percent: Option<f64>,
    window_minutes: Option<f64>,
}

impl Lenient for Window {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut w = Window::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "used_percent" => w.used_percent = map.next_value::<Number>()?.0,
                "window_minutes" => w.window_minutes = map.next_value::<Number>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(w)
    }
}

/// A JSON number (a `bool` counts as 0 or 1, as in Python), else `None`.
#[derive(Default)]
struct Number(Option<f64>);

impl Lenient for Number {
    fn boolean(v: bool) -> Self {
        Number(Some(f64::from(u8::from(v))))
    }
    fn int(v: i64) -> Self {
        Number(Some(v as f64))
    }
    fn uint(v: u64) -> Self {
        Number(Some(v as f64))
    }
    fn float(v: f64) -> Self {
        Number(Some(v))
    }
}

/// A rollout record's `timestamp` as `str(rec.get("timestamp") or "")` reads it: a falsy
/// value is `""`.
#[derive(Default)]
struct Stamp(PyStr);

impl Lenient for Stamp {
    fn text(v: &str) -> Self {
        Stamp(v.into())
    }
    fn boolean(v: bool) -> Self {
        Stamp(if v { "True" } else { "" }.into())
    }
    fn int(v: i64) -> Self {
        Stamp(if v == 0 { String::new() } else { v.to_string() }.into())
    }
    fn uint(v: u64) -> Self {
        Stamp(if v == 0 { String::new() } else { v.to_string() }.into())
    }
    fn float(v: f64) -> Self {
        Stamp(if v == 0.0 { String::new() } else { py_float(v) }.into())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn meter_prefers_the_weekly_window_and_the_latest_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let day = sessions.join("2026/01/02");
        fs::create_dir_all(&day).unwrap();
        assert_eq!(codex_meter(&sessions), Meter::default());
        fs::write(
            day.join("rollout-a.jsonl"),
            "{\"timestamp\":\"2026-01-02T05:00:00.000Z\",\"payload\":{\"rate_limits\":{\
             \"primary\":{\"used_percent\":3,\"window_minutes\":300},\
             \"secondary\":{\"used_percent\":41.5,\"window_minutes\":10080}}}}\n\
             {\"timestamp\":\"2026-01-02T04:00:00.000Z\",\"payload\":{\"rate_limits\":{\
             \"primary\":{\"used_percent\":99,\"window_minutes\":10080}}}}\n\
             {\"payload\":{\"rate_limits\":null}}\n\
             not json rate_limits\n",
        )
        .unwrap();
        let m = codex_meter(&sessions);
        assert_eq!(m.percent, Some(41.5));
        assert_eq!(m.observed_at, Some("2026-01-02T05:00:00.000Z".into()));
        // A matched name that cannot be stat'ed leaves the meter unknown, as in the Python.
        std::os::unix::fs::symlink("/nonexistent", day.join("rollout-dangling.jsonl")).unwrap();
        assert_eq!(codex_meter(&sessions), Meter::default());
    }

    #[test]
    fn fields_of_another_type_are_no_value() {
        #[derive(Deserialize)]
        struct Probe {
            a: Number,
            b: Number,
            c: Window,
            d: Window,
            e: Stamp,
            f: Stamp,
            g: Stamp,
        }
        let p: Probe = serde_json::from_str(
            r#"{"a": true, "b": [1, {"x": 2}], "c": [], "d": {"used_percent": "9", "window_minutes": 10080, "z": {}},
                "e": 0, "f": 1.5, "g": {"k": [1]}}"#,
        )
        .unwrap();
        assert_eq!((p.a.0, p.b.0), (Some(1.0), None));
        assert_eq!(p.c.used_percent, None);
        assert_eq!(
            (p.d.used_percent, p.d.window_minutes),
            (None, Some(10080.0))
        );
        assert_eq!(
            (p.e.0.lossy(), p.f.0.lossy(), p.g.0.lossy()),
            ("".into(), "1.5".into(), "".into())
        );
    }
}
