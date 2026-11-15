//! Python's rules, reproduced where parity depends on them: how `str` splits, strips and
//! `repr`s text, how a `float` prints, how `os.path` resolves and expands a path, and how
//! `shutil.which` finds a program. The tests compare each with the Python itself.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

// --------------------------------------------------------------------------
// Text (`str`)

/// `str.isspace`: Unicode White_Space, plus U+001C..U+001F.
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// `str.strip()` of OS bytes decoded with `surrogateescape`, given back as bytes: an
/// undecodable byte is never whitespace, and a character is whitespace as `py_isspace` says.
/// A valid character at either end is found by its bytes alone, since the decoder never
/// takes a lead byte into the sequence before it.
pub fn py_strip_bytes(s: &[u8]) -> &[u8] {
    let mut start = 0;
    for chunk in s.utf8_chunks() {
        let valid = chunk.valid();
        let kept = valid.trim_start_matches(py_isspace);
        start += valid.len() - kept.len();
        if !kept.is_empty() || !chunk.invalid().is_empty() {
            break;
        }
    }
    let rest = &s[start..];
    let mut end = rest.len();
    loop {
        // The last character, when the bytes end in a valid one.
        let last = (1..=end.min(4)).find_map(|n| {
            let mut chars = std::str::from_utf8(&rest[end - n..end]).ok()?.chars();
            let c = chars.next()?;
            chars.next().is_none().then_some((c, n))
        });
        match last {
            Some((c, n)) if py_isspace(c) => end -= n,
            _ => break,
        }
    }
    &rest[..end]
}

/// `str.split()`.
pub fn py_split(s: &str) -> impl Iterator<Item = &str> {
    s.split(py_isspace).filter(|w| !w.is_empty())
}

/// `str.splitlines()`: lines end at `\n`, `\r\n`, `\r`, `\v`, `\f`, U+001C..U+001E, U+0085,
/// U+2028 and U+2029.
pub fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if boundary {
            out.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' && chars.peek().map(|(_, n)| *n) == Some('\n') {
                chars.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `str.splitlines()` of OS bytes decoded with `surrogateescape`, each line given back as its
/// bytes: the same line ends as `py_splitlines`. An undecodable byte is never one, and the
/// multi-byte ends (U+0085, U+2028, U+2029) start with a lead byte, so they are found by
/// their bytes alone.
pub fn py_splitlines_bytes(s: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < s.len() {
        let end = match &s[i..] {
            [b'\r', b'\n', ..] => 2,
            [b'\n' | b'\r' | 0x0b | 0x0c | 0x1c | 0x1d | 0x1e, ..] => 1,
            [0xc2, 0x85, ..] => 2,
            [0xe2, 0x80, 0xa8 | 0xa9, ..] => 3,
            _ => 0,
        };
        if end == 0 {
            i += 1;
            continue;
        }
        out.push(&s[start..i]);
        i += end;
        start = i;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `repr(s)` for a `str`.
pub fn py_repr(s: &str) -> String {
    py_repr_code_points(s.chars().map(u32::from))
}

/// `repr(s)` for a `str` given as its code points, lone surrogates included (escaped, as
/// Python escapes every unprintable character): `PyStr::repr`.
pub(crate) fn py_repr_code_points(cps: impl Iterator<Item = u32> + Clone) -> String {
    let has = |q: u32| cps.clone().any(|cp| cp == q);
    let quote = if has(u32::from('\'')) && !has(u32::from('"')) {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for cp in cps {
        match char::from_u32(cp) {
            Some('\\') => out.push_str("\\\\"),
            Some('\t') => out.push_str("\\t"),
            Some('\n') => out.push_str("\\n"),
            Some('\r') => out.push_str("\\r"),
            Some(c) if c == quote => {
                out.push('\\');
                out.push(c);
            }
            Some(c) if py_printable(c) => out.push(c),
            _ => match cp {
                0..=0xff => out.push_str(&format!("\\x{cp:02x}")),
                0x100..=0xffff => out.push_str(&format!("\\u{cp:04x}")),
                _ => out.push_str(&format!("\\U{cp:08x}")),
            },
        }
    }
    out.push(quote);
    out
}

/// An approximation of `str.isprintable` for one character: not a control, format, separator
/// (other than the space) or private-use character. Unassigned code points, which Python also
/// escapes, are kept.
fn py_printable(c: char) -> bool {
    let cp = c as u32;
    !(c.is_control()
        || (c.is_whitespace() && c != ' ')
        || matches!(
            cp,
            0xad | 0x600..=0x605
                | 0x61c
                | 0x6dd
                | 0x70f
                | 0x180e
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x2064
                | 0x2066..=0x206f
                | 0xfeff
                | 0xfff9..=0xfffb
                | 0xe000..=0xf8ff
                | 0xf0000..=0xffffd
                | 0x100000..=0x10fffd
        ))
}

/// `repr(x)` (and `str(x)`) for a `float`: the shortest digits that read back, in fixed
/// notation for exponents -4 to 15, else as `d.ddde±XX`.
pub fn py_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("{:e} writes an exponent");
    let exp: i32 = exp.parse().expect("{:e} writes an integer exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let sign = if x < 0.0 { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let point = exp + 1;
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_string()
        } else {
            format!("{first}.{rest}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{esign}{:02}", exp.abs())
    }
}

// --------------------------------------------------------------------------
// Paths (`os.path`)

/// `os.path.realpath(path)` (not strict): absolute, with `.`, `..` and every symlink
/// resolved. A link is followed even when what it names does not exist, so a dangling link
/// resolves to where it points; a symlink loop is left as it stands.
pub fn real_path(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut seen = HashMap::new();
    join_real(PathBuf::from("/"), abs.as_os_str().as_bytes(), &mut seen).0
}

/// `posixpath._joinrealpath`: `rest` resolved onto `path`, and whether it resolved (false
/// inside a symlink loop). `seen` maps each link met to what it resolved to (`None` while it
/// is being resolved).
fn join_real(
    mut path: PathBuf,
    rest: &[u8],
    seen: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> (PathBuf, bool) {
    if rest.starts_with(b"/") {
        path = PathBuf::from("/");
    }
    let names: Vec<&[u8]> = rest.split(|b| *b == b'/').collect();
    for (i, name) in names.iter().enumerate() {
        match *name {
            b"" | b"." => continue,
            b".." => {
                path.pop();
                continue;
            }
            _ => {}
        }
        let next = path.join(OsStr::from_bytes(name));
        let is_link = std::fs::symlink_metadata(&next).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            path = next;
            continue;
        }
        match seen.get(&next) {
            Some(Some(resolved)) => {
                path = resolved.clone();
                continue;
            }
            // A link being resolved: a loop.
            Some(None) => return (pushed(next, &names[i + 1..]), false),
            None => {}
        }
        let Ok(target) = std::fs::read_link(&next) else {
            path = next;
            continue;
        };
        seen.insert(next.clone(), None);
        let (resolved, ok) = join_real(path, target.as_os_str().as_bytes(), seen);
        if !ok {
            return (pushed(resolved, &names[i + 1..]), false);
        }
        seen.insert(next, Some(resolved.clone()));
        path = resolved;
    }
    (path, true)
}

fn pushed(mut path: PathBuf, names: &[&[u8]]) -> PathBuf {
    for name in names.iter().filter(|n| !n.is_empty()) {
        path.push(OsStr::from_bytes(name));
    }
    path
}

/// `os.path.normpath(path)`: repeated slashes, `.` and `name/..` pairs go (a `..` at the
/// root goes too; leading ones of a relative path stay); exactly two leading slashes are
/// kept; an empty result is `.`. No symlink is looked at.
pub fn normpath(path: &OsStr) -> PathBuf {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return PathBuf::from(".");
    }
    let lead: &[u8] = if bytes.starts_with(b"//") && !bytes.starts_with(b"///") {
        b"//"
    } else if bytes.starts_with(b"/") {
        b"/"
    } else {
        b""
    };
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in bytes.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." if !lead.is_empty() || parts.last().is_some_and(|p| *p != b"..") => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut out = lead.to_vec();
    out.extend_from_slice(&parts.join(&b'/'));
    if out.is_empty() {
        out.push(b'.');
    }
    PathBuf::from(OsString::from_vec(out))
}

/// `os.path.abspath(path)`: `path` joined onto the working directory when relative, then
/// `normpath`. Fails as `os.getcwd` fails, when the working directory is gone.
pub fn abspath(path: &Path) -> std::io::Result<PathBuf> {
    if path.as_os_str().as_bytes().starts_with(b"/") {
        return Ok(normpath(path.as_os_str()));
    }
    let mut joined = std::env::current_dir()?.into_os_string().into_vec();
    // `os.path.join`: a separator only where the working directory does not end with one.
    if !joined.ends_with(b"/") {
        joined.push(b'/');
    }
    joined.extend_from_slice(path.as_os_str().as_bytes());
    Ok(normpath(OsStr::from_bytes(&joined)))
}

/// `~` and `~/…` expanded to the home directory (`os.path.expanduser`). `~user` forms are
/// left as they are.
pub fn expand_user(path: &OsStr) -> OsString {
    let bytes = path.as_bytes();
    if !(bytes == b"~" || bytes.starts_with(b"~/")) {
        return path.to_os_string();
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::home_dir().map(PathBuf::into_os_string))
        .unwrap_or_default();
    let mut out = home.as_bytes().to_vec();
    while out.last() == Some(&b'/') {
        out.pop();
    }
    out.extend_from_slice(&bytes[1..]);
    if out.is_empty() {
        out.push(b'/');
    }
    OsString::from_vec(out)
}

// --------------------------------------------------------------------------
// Programs (`shutil.which`)

/// The search path when `PATH` is unset: `os.confstr("CS_PATH")`.
#[cfg(target_os = "macos")]
const DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
#[cfg(not(target_os = "macos"))]
const DEFAULT_PATH: &str = "/bin:/usr/bin";

/// The executable file `name` on `PATH` (`shutil.which`), or `None`.
pub fn which(name: &str) -> Option<PathBuf> {
    which_in(std::env::var_os("PATH").as_deref(), name, Path::new("."))
}

/// `which` with `path` as the value of `PATH` (`None`: unset), a relative candidate (from an
/// empty entry) looked for under `cwd`. The result is named as Python names it: an empty
/// entry's candidate is the bare `name`.
fn which_in(path: Option<&OsStr>, name: &str, cwd: &Path) -> Option<PathBuf> {
    search_dirs(path)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|p| is_executable_file(&cwd.join(p)))
}

/// The directories `which` searches, in order (`shutil.which`, Python 3.12). An unset `PATH`
/// is the OS default; an empty one searches nothing. An empty entry inside a non-empty `PATH`
/// is the current directory, as Python and `execvp` read it (ruling S2-R28). Repeats are
/// searched once.
fn search_dirs(path: Option<&OsStr>) -> Vec<PathBuf> {
    let path = path.unwrap_or(OsStr::new(DEFAULT_PATH));
    if path.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in path.as_bytes().split(|b| *b == b':') {
        let dir = PathBuf::from(OsStr::from_bytes(dir));
        if !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

fn is_executable_file(p: &Path) -> bool {
    p.exists() && !p.is_dir() && rustix::fs::access(p, rustix::fs::Access::EXEC_OK).is_ok()
}

/// `str(PurePosixPath(path))`: a path as `pathlib` holds it, so a path printed back is the
/// Python's spelling. Repeated and trailing slashes and `.` components go (`..` stays); an
/// empty path is `.`; exactly two leading slashes are kept, as POSIX allows.
pub fn pure_path(path: &OsStr) -> PathBuf {
    let bytes = path.as_bytes();
    let lead: &[u8] = if bytes.starts_with(b"//") && !bytes.starts_with(b"///") {
        b"//"
    } else if bytes.starts_with(b"/") {
        b"/"
    } else {
        b""
    };
    let parts: Vec<&[u8]> = bytes
        .split(|b| *b == b'/')
        .filter(|p| !p.is_empty() && *p != b".")
        .collect();
    let mut out = lead.to_vec();
    out.extend_from_slice(&parts.join(&b'/'));
    if out.is_empty() {
        out.push(b'.');
    }
    PathBuf::from(OsString::from_vec(out))
}

/// `str(Path(p))` as text the Python holds: the `pathlib` spelling (`pure_path`), its
/// undecodable bytes surrogate-escaped (`PyStr::from_os`).
pub fn path_text(p: &Path) -> crate::wire::PyStr {
    crate::wire::PyStr::from_os(pure_path(p.as_os_str()).as_os_str())
}

/// `f"{x:.{digits}f}"`: fixed-point, correctly rounded, ties to even on the exact value, as
/// Python formats it.
pub fn py_fixed(x: f64, digits: usize) -> String {
    format!("{x:.digits$}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn pure_path_is_pathlib_spelling() {
        let cases = [
            "//a//b/./c/",
            "///a",
            "",
            "./a",
            "a/..",
            ".",
            "a/./",
            "/",
            "//",
            "a//b",
            "/runs/2026-11-10-retry uploads/",
        ];
        let theirs = python_map(
            "__import__('pathlib').PurePosixPath(x).__str__()",
            &strings(&cases),
        );
        for (case, want) in cases.iter().zip(theirs) {
            assert_eq!(
                pure_path(OsStr::new(case)),
                Path::new(want.as_str().unwrap()),
                "{case:?}"
            );
        }
        assert_eq!(
            pure_path(OsStr::from_bytes(b"./r\xff//"))
                .as_os_str()
                .as_bytes(),
            b"r\xff"
        );
    }

    #[test]
    fn fixed_point_matches_python() {
        let xs = [
            0.25,
            0.125,
            2.5,
            0.35,
            1.005,
            0.045,
            12.25,
            0.0,
            -0.0,
            1e-9,
            123456.785,
            99.95,
            0.000_123_456_7,
            31.0,
            1e21,
        ];
        let inputs: Vec<serde_json::Value> = xs.iter().map(|x| serde_json::json!(x)).collect();
        let theirs = python_map("[f'{x:.1f}', f'{x:.2f}', f'{x:.6f}']", &inputs);
        for (x, want) in xs.iter().zip(theirs) {
            let ours = serde_json::json!([py_fixed(*x, 1), py_fixed(*x, 2), py_fixed(*x, 6)]);
            assert_eq!(ours, want, "{x:e}");
        }
    }

    /// `expr` (a Python expression over `x`) applied by `python3` to each input; the results
    /// as JSON values.
    pub(crate) fn python_map(expr: &str, inputs: &[serde_json::Value]) -> Vec<serde_json::Value> {
        python_eval("", expr, inputs)
    }

    /// `python_map`, after `prelude` (Python statements, with the reference's `leadhand`
    /// package importable).
    pub(crate) fn python_eval(
        prelude: &str,
        expr: &str,
        inputs: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        let script = format!(
            "import json, sys\n{prelude}\n\
             print(json.dumps([(lambda x: {expr})(x) for x in json.load(sys.stdin)]))"
        );
        let mut child = std::process::Command::new("python3")
            .args(["-B", "-c", &script])
            .env(
                "PYTHONPATH",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/lib"),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("python3 runs");
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().unwrap();
            stdin
                .write_all(serde_json::to_string(inputs).unwrap().as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn strings(items: &[&str]) -> Vec<serde_json::Value> {
        items.iter().map(|s| serde_json::json!(s)).collect()
    }

    const TEXTS: [&str; 16] = [
        "",
        "plain",
        "it's",
        "say \"hi\"",
        "both ' and \"",
        "back\\slash",
        "tab\tnew\nline\rret",
        "\u{0}\u{7}\u{1b}\u{7f}",
        "caf\u{e9} \u{2192} \u{1f600}",
        "\u{85}\u{a0}\u{2028}\u{3000}",
        "\u{ad}\u{200b}\u{feff}\u{e000}",
        "  \t lead and trail \u{1c}\u{1f} ",
        "a\r\nb\rc\nd\u{b}e\u{c}f\u{1c}g\u{1d}h\u{1e}i\u{85}j\u{2028}k\u{2029}l",
        "trailing newline\n",
        "\n\nblank lines\n\n",
        "x\u{1f}y z",
    ];

    #[test]
    fn repr_matches_python() {
        let theirs = python_map("repr(x)", &strings(&TEXTS));
        for (text, want) in TEXTS.iter().zip(theirs) {
            assert_eq!(py_repr(text), want.as_str().unwrap(), "{text:?}");
        }
    }

    #[test]
    fn strip_split_and_splitlines_match_python() {
        let theirs = python_map("[x.strip(), x.split(), x.splitlines()]", &strings(&TEXTS));
        for (text, want) in TEXTS.iter().zip(theirs) {
            let ours = serde_json::json!([
                py_strip(text),
                py_split(text).collect::<Vec<_>>(),
                py_splitlines(text)
            ]);
            assert_eq!(ours, want, "{text:?}");
        }
    }

    #[test]
    fn strip_bytes_matches_python_surrogateescape_strip() {
        let mut cases: Vec<Vec<u8>> = TEXTS.iter().map(|t| t.as_bytes().to_vec()).collect();
        cases.extend([
            b"\xff refs/x \n".to_vec(),
            b" \xc2\xa0main\xff\xc2\xa0\n".to_vec(),
            b"\xe2\xc2\x85x\xe2\x80\x85".to_vec(),
            b"\xe2\x80\n".to_vec(),
            b"\xc2".to_vec(),
        ]);
        // Each case as its bytes; Python decodes, strips and encodes back.
        let inputs: Vec<serde_json::Value> = cases.iter().map(|c| serde_json::json!(c)).collect();
        let theirs = python_map(
            "list(bytes(x).decode('utf-8', 'surrogateescape').strip()\
             .encode('utf-8', 'surrogateescape'))",
            &inputs,
        );
        for (case, want) in cases.iter().zip(theirs) {
            assert_eq!(serde_json::json!(py_strip_bytes(case)), want, "{case:?}");
        }
    }

    #[test]
    fn splitlines_of_os_bytes_matches_python() {
        let cases: [&[u8]; 9] = [
            b"worktree /a\nHEAD x\n\nworktree /b c\n",
            b"a\r\nb\rc\x0bd\x0ce\x1cf\x1dg\x1eh",
            b"one \xc2\x85 two \xe2\x80\xa8 three \xe2\x80\xa9 four",
            b"\xff\n\xfe\xc2\x85\xe2\x80",
            b"\xe2\x80\xe2\x80\xa8x",
            b"\xc2\xc2\x85",
            b"",
            b"\n\n",
            b"no end",
        ];
        let code = "import json, os, sys\n\
                    print(json.dumps([[os.fsencode(l).hex() for l in os.fsdecode(bytes.fromhex(a)).splitlines()]\n\
                    \x20 for a in sys.argv[1:]]))\n";
        let hexes: Vec<String> = cases
            .iter()
            .map(|c| c.iter().map(|b| format!("{b:02x}")).collect())
            .collect();
        let args: Vec<&OsStr> = hexes.iter().map(OsStr::new).collect();
        let out = crate::wire::tests::python(code, &args);
        let theirs: Vec<Vec<String>> = serde_json::from_slice(&out).unwrap();
        for (case, want) in cases.iter().zip(theirs) {
            let ours: Vec<String> = py_splitlines_bytes(case)
                .iter()
                .map(|l| l.iter().map(|b| format!("{b:02x}")).collect())
                .collect();
            assert_eq!(ours, want, "{case:?}");
        }
    }

    #[test]
    fn float_repr_matches_python() {
        let xs = [
            0.1,
            2.5,
            1e-5,
            1e-4,
            0.00012,
            5.0,
            1e15,
            1e16,
            1e20,
            123456.789,
            1.0 / 3.0,
            -2.5,
            -0.0,
            0.0,
            9.99e-5,
            1.5e300,
            5e-324,
        ];
        let inputs: Vec<serde_json::Value> = xs.iter().map(|x| serde_json::json!(x)).collect();
        let theirs = python_map("repr(float(x))", &inputs);
        for (x, want) in xs.iter().zip(theirs) {
            assert_eq!(py_float(*x), want.as_str().unwrap(), "{x:e}");
        }
        assert_eq!(
            (
                py_float(f64::NAN),
                py_float(f64::INFINITY),
                py_float(-f64::INFINITY)
            ),
            ("nan".into(), "inf".into(), "-inf".into())
        );
    }

    #[test]
    fn real_path_follows_links_as_python_does() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("repo/sub")).unwrap();
        symlink(root.join("repo/notes/docs"), root.join("dangling")).unwrap();
        symlink("repo/sub", root.join("relative")).unwrap();
        symlink(root.join("loop-b"), root.join("loop-a")).unwrap();
        symlink(root.join("loop-a"), root.join("loop-b")).unwrap();
        symlink("../repo", root.join("repo/sub/up")).unwrap();
        let cases = [
            "dangling",
            "dangling/deeper",
            "relative",
            "relative/../x",
            "relative/up/sub",
            "loop-a",
            "loop-a/x",
            "repo/./sub//",
            "missing/../repo",
            "repo/sub/up/sub/up",
        ];
        let inputs: Vec<serde_json::Value> = cases
            .iter()
            .map(|c| serde_json::json!(root.join(c).to_str().unwrap()))
            .collect();
        let theirs = python_map("__import__('os').path.realpath(x)", &inputs);
        for (case, want) in cases.iter().zip(theirs) {
            assert_eq!(
                real_path(&root.join(case)).to_str().unwrap(),
                want.as_str().unwrap(),
                "{case}"
            );
        }
        // The point of following a dangling link: it names a place inside the repository.
        assert!(real_path(&root.join("dangling")).starts_with(root.join("repo")));
    }

    #[test]
    fn which_matches_python_for_empty_entries_and_an_empty_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (bin, cwd) = (root.join("bin"), root.join("cwd"));
        for d in [&bin, &cwd] {
            std::fs::create_dir(d).unwrap();
        }
        let exec = |p: &Path| {
            std::fs::write(p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        exec(&bin.join("in-bin"));
        exec(&cwd.join("in-cwd"));
        std::fs::write(bin.join("not-exec"), "x").unwrap();
        let b = bin.display().to_string();
        let cases: Vec<(String, &str)> = vec![
            (format!("/nonexistent:{b}"), "in-bin"),
            (b.clone(), "not-exec"),
            (b.clone(), "in-cwd"),
            // An empty entry inside a non-empty PATH is the current directory (S2-R28).
            (format!("/nonexistent::{b}"), "in-cwd"),
            (format!("{b}:"), "in-cwd"),
            (":".into(), "in-cwd"),
            (format!(":{b}"), "in-bin"),
            // An empty PATH searches nothing, not even the current directory.
            (String::new(), "in-cwd"),
            (String::new(), "in-bin"),
        ];
        let script = "import json, shutil, sys\n\
                      print(json.dumps([shutil.which(n, path=p) for p, n in json.load(sys.stdin)]))";
        let mut child = std::process::Command::new("python3")
            .args(["-B", "-c", script])
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            let input = serde_json::to_string(&cases).unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        let theirs: Vec<Option<String>> = serde_json::from_slice(&out.stdout).unwrap();
        for ((path, name), want) in cases.iter().zip(theirs) {
            let ours = which_in(Some(OsStr::new(path)), name, &cwd);
            assert_eq!(
                ours.map(|p| p.to_str().unwrap().to_string()),
                want,
                "PATH={path:?} {name}"
            );
        }
        // Repeats are searched once; an empty entry is kept, in its place.
        assert_eq!(
            search_dirs(Some(OsStr::new(":/usr/bin::/bin:/usr/bin:"))),
            [
                PathBuf::from(""),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin")
            ]
        );
        // An unset PATH is the OS default (`os.confstr("CS_PATH")`).
        let theirs = python_map(
            "__import__('os').confstr('CS_PATH')",
            &[serde_json::json!(0)],
        );
        assert_eq!(theirs[0].as_str(), Some(DEFAULT_PATH));
        assert_eq!(
            search_dirs(None),
            DEFAULT_PATH
                .split(':')
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn expand_user_is_expanduser_for_tilde_forms() {
        let home = std::env::var_os("HOME").unwrap_or_default();
        let cases = ["~", "~/runs/{repo}", "/abs/~", "rel", "~other/x"];
        let inputs = strings(&cases);
        let theirs = python_map("__import__('os').path.expanduser(x)", &inputs);
        for (case, want) in cases.iter().zip(theirs) {
            if case.starts_with("~other") {
                // `~user` is left as it is (no passwd lookup); Python expands it when the user
                // exists, and there is no user `other` here.
                assert_eq!(expand_user(OsStr::new(case)), OsString::from(case));
                continue;
            }
            assert_eq!(
                expand_user(OsStr::new(case)).to_str().unwrap(),
                want.as_str().unwrap(),
                "{case} (HOME={home:?})"
            );
        }
    }

    #[test]
    fn normpath_and_abspath_match_python() {
        let cases = [
            "",
            ".",
            "/",
            "//",
            "///",
            "////x",
            "//x/../y",
            "a/b/../../..",
            "../a/./b//c/",
            "/../x",
            "/a/b/../../../c",
            "a/..",
            "./",
            "x/./y/.",
            "/runs/r 1/workers/c1-a1",
            "caf\u{e9}/../d",
            "relative/dir",
        ];
        let inputs = strings(&cases);
        let norm = python_map("__import__('os').path.normpath(x)", &inputs);
        let abs = python_map("__import__('os').path.abspath(x)", &inputs);
        for ((case, want_norm), want_abs) in cases.iter().zip(norm).zip(abs) {
            assert_eq!(
                normpath(OsStr::new(case)).to_str().unwrap(),
                want_norm.as_str().unwrap(),
                "normpath {case:?}"
            );
            assert_eq!(
                abspath(Path::new(case)).unwrap().to_str().unwrap(),
                want_abs.as_str().unwrap(),
                "abspath {case:?}"
            );
        }
        // Bytes that are not UTF-8 pass through untouched.
        assert_eq!(
            normpath(OsStr::from_bytes(b"/a/\xff/./b/.."))
                .as_os_str()
                .as_bytes(),
            b"/a/\xff"
        );
    }
}
