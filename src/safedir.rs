//! `SafeDir`: the only way to touch files a worker can write (spec, bug class 5): scratch
//! folders, briefs, reports, reply files and the workspace (`briefs._open_scratch`,
//! `read_scratch_file`, `write_scratch_file`, `remove_scratch_file`).
//!
//! The CLI runs outside Claude Code's sandbox, and a worker can plant a symlink, a file or a
//! FIFO where leadhand will look later. So a folder is reached from a trusted root by opening
//! each component with `openat(O_DIRECTORY | O_NOFOLLOW)` relative to the one before, and
//! every file operation acts relative to the last folder's descriptor: nothing is followed
//! out of the tree, even when a folder is swapped after the check. A file is opened with
//! `O_NOFOLLOW | O_NONBLOCK`, and `fstat` must show a regular file within a size cap, so a
//! symlink, FIFO, socket or device is refused without blocking.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, fstat, mkdirat, openat, renameat, statat, unlinkat,
};
use rustix::io::Errno;

use crate::error::{Error, Result};
use crate::store::{TEMP_TRIES, fsync, temp_name};

/// How a folder below the root is opened: never through a symlink.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// An open folder reached without following a symlink.
pub struct SafeDir {
    fd: OwnedFd,
    /// For messages only.
    path: PathBuf,
}

impl SafeDir {
    /// The folder `root/rel…`, or `None` when a component is missing. `root` is trusted and
    /// may be reached through symlinks; a component below it that is a symlink or not a folder
    /// is refused, with the path.
    pub fn open_under(root: &Path, rel: &[&OsStr]) -> Result<Option<SafeDir>> {
        walk(root, rel, false)
    }

    /// As `open_under`, creating each missing component.
    pub fn create_under(root: &Path, rel: &[&OsStr]) -> Result<SafeDir> {
        walk(root, rel, true).map(|dir| dir.expect("a creating walk makes every folder"))
    }

    /// The bytes of the regular file `name` here, at most `max` of them; `None` when it is
    /// missing. A symlink, or anything but a regular file, is an error.
    pub fn read_file(&self, name: &OsStr, max: u64) -> Result<Option<Vec<u8>>> {
        self.read_file_checked(name, max).map_err(Error::from)
    }

    /// As `read_file`, with why it failed kept apart (`FileProblem`), for a caller that words
    /// the failure itself (the engine notes a brief or report it did not copy, with the OS's
    /// own reason, as the Python notes `strerror`).
    pub fn read_file_checked(
        &self,
        name: &OsStr,
        max: u64,
    ) -> std::result::Result<Option<Vec<u8>>, FileProblem> {
        let shown = self.child(name).map_err(FileProblem::Name)?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = match openat(&self.fd, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(FileProblem::Os(shown, e.into())),
        };
        let stat = match fstat(&fd) {
            Ok(stat) => stat,
            Err(e) => return Err(FileProblem::Os(shown, e.into())),
        };
        if !FileType::from_raw_mode(stat.st_mode).is_file() {
            return Err(FileProblem::NotRegular(shown));
        }
        if u64::try_from(stat.st_size).unwrap_or(0) > max {
            return Err(FileProblem::TooLarge(shown, max));
        }
        let mut data = Vec::new();
        if let Err(e) = File::from(fd)
            .take(max.saturating_add(1))
            .read_to_end(&mut data)
        {
            return Err(FileProblem::Os(shown, e));
        }
        if data.len() as u64 > max {
            return Err(FileProblem::TooLarge(shown, max));
        }
        Ok(Some(data))
    }

    /// Replaces `name` here with `data`, atomically (temporary file, fsync, rename). A symlink
    /// named `name` is replaced, never followed.
    pub fn write_file(&self, name: &OsStr, data: &[u8]) -> Result<()> {
        let shown = self.child(name)?;
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut tries = 0;
        let (tmp, fd) = loop {
            let tmp = temp_name(name);
            match openat(&self.fd, &tmp, flags, Mode::from_raw_mode(0o644)) {
                Ok(fd) => break (tmp, fd),
                Err(Errno::EXIST) if tries < TEMP_TRIES => tries += 1,
                Err(e) => return Err(Error::io(&self.path.join(&tmp), e.into())),
            }
        };
        let written = (|| {
            let mut file = File::from(fd);
            file.write_all(data).map_err(|e| Error::io(&shown, e))?;
            fsync(&file, &shown)?;
            drop(file);
            renameat(&self.fd, &tmp, &self.fd, name).map_err(|e| Error::io(&shown, e.into()))
        })();
        if written.is_err() {
            let _ = unlinkat(&self.fd, &tmp, AtFlags::empty());
        }
        written
    }

    /// Removes `name` here if it is there; a symlink is removed itself, never its target.
    pub fn remove_file(&self, name: &OsStr) -> Result<()> {
        let shown = self.child(name)?;
        match unlinkat(&self.fd, name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(Error::io(&shown, e.into())),
        }
    }

    /// The path of `name` here, for messages, once `name` is known to be one component.
    fn child(&self, name: &OsStr) -> Result<PathBuf> {
        one_component(name, &self.path)?;
        Ok(self.path.join(name))
    }
}

/// Why `SafeDir::read_file_checked` read nothing: each names the file.
#[derive(Debug)]
pub enum FileProblem {
    /// The name is not one path component.
    Name(Error),
    /// The OS refused (a symlink is `ELOOP`), or reading failed.
    Os(PathBuf, std::io::Error),
    /// Not a regular file (a FIFO, a socket, a folder, a device).
    NotRegular(PathBuf),
    /// Larger than the cap.
    TooLarge(PathBuf, u64),
}

impl From<FileProblem> for Error {
    /// As `read_file` words it.
    fn from(p: FileProblem) -> Error {
        match p {
            FileProblem::Name(e) => e,
            FileProblem::Os(path, e) => Error::io(&path, e),
            FileProblem::NotRegular(path) => {
                Error::msg(format!("not a regular file: '{}'", path.display()))
            }
            FileProblem::TooLarge(path, max) => {
                Error::msg(format!("larger than {max} bytes: '{}'", path.display()))
            }
        }
    }
}

/// Opens `root`, then each of `rel` below it without following a symlink, creating a missing
/// one when `create` (`briefs._open_scratch`).
fn walk(root: &Path, rel: &[&OsStr], create: bool) -> Result<Option<SafeDir>> {
    let mut fd = rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| Error::io(root, e.into()))?;
    let mut path = root.to_path_buf();
    for &name in rel {
        one_component(name, &path)?;
        path.push(name);
        fd = match openat(&fd, name, DIR_FLAGS, Mode::empty()) {
            Ok(next) => next,
            Err(Errno::NOENT) if !create => return Ok(None),
            Err(Errno::NOENT) => {
                match mkdirat(&fd, name, Mode::from_raw_mode(0o777)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(Error::io(&path, e.into())),
                }
                openat(&fd, name, DIR_FLAGS, Mode::empty())
                    .map_err(|e| refusal(&fd, name, &path, e))?
            }
            Err(e) => return Err(refusal(&fd, name, &path, e)),
        };
    }
    Ok(Some(SafeDir { fd, path }))
}

/// Why `name` in `dir` could not be opened as a folder: a symlink or something else in the
/// way is refused (`briefs._refusal`); any other error is itself.
fn refusal(dir: &OwnedFd, name: &OsStr, shown: &Path, e: Errno) -> Error {
    if ![Errno::LOOP, Errno::NOTDIR, Errno::NOENT].contains(&e) {
        return Error::io(shown, e.into());
    }
    let symlink = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|st| FileType::from_raw_mode(st.st_mode).is_symlink());
    let what = if symlink { "a symlink" } else { "not a folder" };
    Error::msg(format!(
        "refused: the path {} is {what}; remove it and run the command again \
         (a worker may have planted it)",
        shown.display()
    ))
}

/// `name` must be one path component, so an `openat` relative to a folder stays in it.
fn one_component(name: &OsStr, under: &Path) -> Result<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(Error::msg(format!(
            "not a single path component under {}: {:?}",
            under.display(),
            name.to_string_lossy()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn safedir_refuses_symlinked_component() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, outside) = (tmp.path().join("ws"), tmp.path().join("outside"));
        fs::create_dir_all(ws.join(".leadhand/1")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("report.md"), b"keep me\n").unwrap();
        symlink(&outside, ws.join(".leadhand/1/c1-a1-review-1")).unwrap();
        let rel = [os(".leadhand"), os("1"), os("c1-a1-review-1")];
        let shown = ws.join(".leadhand/1/c1-a1-review-1");
        let want = format!(
            "refused: the path {} is a symlink; remove it and run the command again \
             (a worker may have planted it)",
            shown.display()
        );
        assert_eq!(SafeDir::open_under(&ws, &rel).err().unwrap().message, want);
        assert_eq!(
            SafeDir::create_under(&ws, &rel).err().unwrap().message,
            want
        );
        // A symlinked folder higher up is refused as well, and nothing is made through it.
        fs::rename(ws.join(".leadhand/1"), ws.join(".leadhand/1-moved")).unwrap();
        symlink(&outside, ws.join(".leadhand/1")).unwrap();
        let deeper = [os(".leadhand"), os("1"), os("c1-a1-review-2")];
        let err = SafeDir::create_under(&ws, &deeper).err().unwrap();
        assert!(err.message.contains(" is a symlink;"), "{err}");
        assert!(
            err.message
                .contains(&*ws.join(".leadhand/1").to_string_lossy()),
            "{err}"
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        // A file where a folder should be is "not a folder".
        fs::remove_file(ws.join(".leadhand/1")).unwrap();
        fs::create_dir(ws.join(".leadhand/1")).unwrap();
        fs::write(ws.join(".leadhand/1/c1-a1-review-1"), b"not a folder\n").unwrap();
        let err = SafeDir::open_under(&ws, &rel).err().unwrap();
        assert!(err.message.contains(" is not a folder;"), "{err}");
        // A missing folder is None, or made.
        let fresh = [os(".leadhand"), os("run"), os("run-review-1")];
        assert!(SafeDir::open_under(&ws, &fresh).unwrap().is_none());
        SafeDir::create_under(&ws, &fresh).unwrap();
        assert!(ws.join(".leadhand/run/run-review-1").is_dir());
        assert!(SafeDir::open_under(&ws, &fresh).unwrap().is_some());
        // A component that would walk anywhere else is refused.
        assert!(SafeDir::open_under(&ws, &[os("..")]).is_err());
        assert!(SafeDir::open_under(&ws, &[os(".leadhand/1")]).is_err());
    }

    #[test]
    fn safedir_refuses_fifo_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        let status = Command::new("mkfifo")
            .arg(ws.join("reply.txt"))
            .status()
            .unwrap();
        assert!(status.success());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let dir = SafeDir::open_under(&ws, &[]).unwrap().unwrap();
            tx.send(dir.read_file(os("reply.txt"), 1 << 20)).unwrap();
        });
        let read = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("reading a FIFO blocked");
        let err = read.unwrap_err();
        assert_eq!(
            err.message,
            format!(
                "not a regular file: '{}'",
                tmp.path().join("reply.txt").display()
            )
        );
    }

    #[test]
    fn safedir_files_never_follow_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, outside) = (tmp.path().join("ws"), tmp.path().join("outside"));
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&ws).unwrap();
        fs::write(outside.join("target"), b"keep me\n").unwrap();
        let dir = SafeDir::create_under(&ws, &[os(".leadhand"), os("2"), os("c2-a1-implement-1")])
            .unwrap();
        let here = ws.join(".leadhand/2/c2-a1-implement-1");

        // Read: missing is None; a regular file is read; a symlink is refused; so is a file
        // over the cap.
        assert_eq!(dir.read_file(os("report.md"), 100).unwrap(), None);
        fs::write(here.join("report.md"), b"done\n").unwrap();
        assert_eq!(
            dir.read_file(os("report.md"), 100).unwrap().as_deref(),
            Some(&b"done\n"[..])
        );
        let err = dir.read_file(os("report.md"), 4).unwrap_err();
        assert!(err.message.contains("larger than 4 bytes"), "{err}");
        symlink(outside.join("target"), here.join("reply.txt")).unwrap();
        let err = dir.read_file(os("reply.txt"), 100).unwrap_err();
        assert!(
            err.message
                .contains(&*here.join("reply.txt").to_string_lossy()),
            "{err}"
        );

        // Write replaces the symlink itself, and leaves no temporary file.
        dir.write_file(os("reply.txt"), b"mine\n").unwrap();
        assert!(!here.join("reply.txt").is_symlink());
        assert_eq!(fs::read(here.join("reply.txt")).unwrap(), b"mine\n");
        assert_eq!(fs::read(outside.join("target")).unwrap(), b"keep me\n");
        let mut names: Vec<_> = fs::read_dir(&here)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, [os("reply.txt"), os("report.md")]);

        // Remove takes a symlink away, never its target; a missing file is fine.
        symlink(outside.join("target"), here.join("brief.md")).unwrap();
        dir.remove_file(os("brief.md")).unwrap();
        dir.remove_file(os("brief.md")).unwrap();
        assert!(!here.join("brief.md").exists());
        assert_eq!(fs::read(outside.join("target")).unwrap(), b"keep me\n");

        // A name that is not one component is refused.
        assert!(dir.read_file(os("../x"), 100).is_err());
        assert!(dir.write_file(os(".."), b"").is_err());
    }
}
