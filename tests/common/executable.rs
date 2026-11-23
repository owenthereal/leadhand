//! `write_executable`: writes a file that a test will run.
//!
//! - **The race.** Every test binary runs its tests on parallel threads, and a thread that
//!   starts a process forks. Until its own exec, the forked child holds a copy of every
//!   descriptor the test process has open, close-on-exec ones included. So if a test has a
//!   file open for writing when a sibling thread forks, the child holds a write handle on that
//!   file until it execs.
//! - **Linux refuses to run a file that any process holds open for writing** ("Text file
//!   busy", ETXTBSY), so a test that writes a file and then runs it, or has a child run it, can
//!   fail at random there. macOS runs the file anyway. The race failed `v0.2.1`'s first release
//!   attempt: a `shim` test ran the shim copy it had just written, and Linux refused.
//! - **The rule, in both test builds** (the integration tests and the library's unit tests):
//!   every file a test writes to be run, by the test itself or by any child it starts, is
//!   written with `write_executable`. The test process then never holds such a file open for
//!   writing, so no fork of any of its threads can inherit a write handle on it.
//!
//! The library's unit tests include this file too (`#[path]`), so it uses only `std`: nothing
//! from the harness or the crate's library.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

/// Writes exactly `contents` to `path`, creating or truncating it (its folder must exist), and
/// sets its mode to 0755. Returns only once the writer has exited successfully; panics on any
/// failure, naming `path`, and for a writer that failed, its exit status and its stderr.
///
/// The writer is `/bin/sh -c 'exec cat > "$1"' sh <path>`: the path is an argument, never in
/// the script's text; the contents go to its stdin; its environment holds only
/// `PATH=/usr/bin:/bin`. It is the only process that ever holds `path` open for writing, and it
/// has exited before the mode is set (`chmod`, which opens nothing). This function never opens
/// `path` itself.
#[track_caller]
pub fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
    let contents = contents.as_ref();
    let mut writer = match Command::new("/bin/sh")
        .args(["-c", "exec cat > \"$1\"", "sh"])
        .arg(path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(writer) => writer,
        Err(e) => panic!(
            "writing the executable {}: starting /bin/sh: {e}",
            path.display()
        ),
    };
    let Some(mut stdin) = writer.stdin.take() else {
        panic!(
            "writing the executable {}: the writer has no stdin",
            path.display()
        );
    };
    // The contents go in on a thread of their own while `wait_with_output` reads the writer's
    // stderr, so a writer that fails before it has read them all can't deadlock this. The
    // thread's end closes the writer's stdin, the end of the contents.
    let (fed, output) = thread::scope(|scope| {
        let feeder = scope.spawn(move || stdin.write_all(contents));
        let output = writer.wait_with_output();
        (feeder.join(), output)
    });
    let output = match output {
        Ok(output) => output,
        Err(e) => panic!(
            "writing the executable {}: waiting for the writer: {e}",
            path.display()
        ),
    };
    if !output.status.success() {
        panic!(
            "writing the executable {}: the writer failed ({}): {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    match fed {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!(
            "writing the executable {}: feeding the writer: {e}",
            path.display()
        ),
        Err(_) => panic!(
            "writing the executable {}: the thread feeding the writer panicked",
            path.display()
        ),
    }
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o755)) {
        panic!("making {} executable: {e}", path.display());
    }
}
