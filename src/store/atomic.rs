//! `write_json_atomic`: a JSON file replaced whole (`state.write_json_atomic`), for
//! `run.json`, `status.json` and the worker files.
//!
//! The JSON goes to a temporary file in the same directory, which is fsynced and renamed over
//! the target; then the directory is fsynced. No temporary file survives, on success or on
//! error.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::store::{TEMP_TRIES, fsync, fsync_dir, parent, temp_name};
use crate::wire;

/// Replaces `path` with `value` as indented ASCII JSON and a final newline.
pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let mut data = wire::to_pretty(value)?.into_bytes();
    data.push(b'\n');
    let dir = parent(path);
    let (tmp, file) = create_temp(dir, path)?;
    let replaced = (|| {
        (&file).write_all(&data).map_err(|e| Error::io(&tmp, e))?;
        fsync(&file, &tmp)?;
        drop(file);
        fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
    })();
    if let Err(e) = replaced {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fsync_dir(dir)
}

/// A new temporary file beside `path`, created `0600` as `mkstemp` creates it.
fn create_temp(dir: &Path, path: &Path) -> Result<(PathBuf, File)> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::msg(format!("not a file path: {}", path.display())))?;
    let mut tries = 0;
    loop {
        let tmp = dir.join(temp_name(name));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists && tries < TEMP_TRIES => tries += 1,
            Err(e) => return Err(Error::io(&tmp, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde::{Serialize, Serializer};

    use super::*;
    use crate::store::FSYNCS;

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    #[derive(Serialize)]
    struct Flag {
        ok: bool,
    }

    #[test]
    fn write_json_atomic_error_keeps_old_content_and_no_temp() {
        struct Unserialisable;
        impl Serialize for Unserialisable {
            fn serialize<S: Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom(
                    "Object of type object is not JSON serializable",
                ))
            }
        }
        #[derive(Serialize)]
        struct Bad {
            bad: Unserialisable,
        }
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("status.json");
        write_json_atomic(&target, &Flag { ok: true }).unwrap();
        assert!(
            write_json_atomic(
                &target,
                &Bad {
                    bad: Unserialisable
                }
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "{\n  \"ok\": true\n}\n"
        );
        assert_eq!(names(tmp.path()), ["status.json"]);
    }

    #[test]
    fn write_json_atomic_replace_failure_cleans_up() {
        // The rename fails: a non-empty folder is in the way.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("status.json");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"").unwrap();
        let err = write_json_atomic(&target, &Flag { ok: true }).unwrap_err();
        assert!(err.message.contains(&*target.to_string_lossy()), "{err}");
        assert_eq!(names(tmp.path()), ["status.json"]);
        assert_eq!(names(&target), ["keep"]);
    }

    #[test]
    fn write_json_atomic_fsyncs() {
        let tmp = tempfile::tempdir().unwrap();
        let before = FSYNCS.get();
        write_json_atomic(&tmp.path().join("x.json"), &Flag { ok: false }).unwrap();
        // The file, then its folder.
        assert!(FSYNCS.get() - before >= 2);
        assert_eq!(names(tmp.path()), ["x.json"]);
    }
}
