//! `$EDITOR` for a value too long, or with too many lines, for the key detail's inline
//! prompt (spec "Edits on the Key detail"). The runtime runs an [`EditorJob`] and answers
//! with `Msg::Edited`.
//!
//! The value is written to a file of its own (mode 0600) in a fresh directory (mode
//! 0700) under the system's temporary directory, the editor runs on it, the file is read
//! back, and the directory is deleted whatever happened: a guard removes it on every
//! return and on a panic. The value never goes anywhere else: no log, no error text.
//!
//! While the editor runs, the TUI leaves the alternate screen and raw mode, and the
//! input thread stops reading the terminal ([`pause_input`]) so every key reaches the
//! editor; then both come back.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::value::Raw;

/// What to edit. `Debug` shows the value's length only ([`Raw`]).
#[derive(Debug, Clone, PartialEq)]
pub struct EditorJob {
    /// Which edit this is, for the view that asked.
    pub id: u64,
    pub value: Raw,
    /// The temporary file's extension, `txt` or `json`, so the editor highlights it.
    pub ext: &'static str,
}

/// True while the editor runs: the input thread does not read the terminal.
pub(super) static INPUT_PAUSED: AtomicBool = AtomicBool::new(false);
/// Held by the input thread while it waits for input, and by the editor while it runs.
pub(super) static INPUT_GATE: Mutex<()> = Mutex::new(());

/// Stops the input thread reading the terminal until the guard is dropped. Waits for
/// the read in progress (at most one poll) to end.
pub(super) fn pause_input() -> InputPause {
    INPUT_PAUSED.store(true, Ordering::SeqCst);
    let gate = INPUT_GATE.lock().unwrap_or_else(|e| e.into_inner());
    InputPause { _gate: gate }
}

pub(super) struct InputPause {
    _gate: MutexGuard<'static, ()>,
}

impl Drop for InputPause {
    fn drop(&mut self) {
        INPUT_PAUSED.store(false, Ordering::SeqCst);
    }
}

/// The editor's command: `$VISUAL`, else `$EDITOR`, else `vi`, split at spaces (so
/// `code -w` works; quoting is not understood).
pub fn editor_command(visual: Option<&str>, editor: Option<&str>) -> Vec<String> {
    let pick = [visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("vi");
    pick.split_whitespace().map(str::to_string).collect()
}

/// A fresh 0700 directory holding the value's 0600 file. Dropping it deletes both.
#[derive(Debug)]
pub struct TempValue {
    dir: PathBuf,
    file: PathBuf,
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

impl TempValue {
    /// Makes the directory under `base` and writes `bytes` to the file in it.
    pub fn create(base: &Path, bytes: &[u8], ext: &str) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let mut last = None;
        for _ in 0..16 {
            let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let dir = base.join(format!(
                "redis-lens-edit-{}-{nanos:08x}-{n}",
                std::process::id()
            ));
            match make_private_dir(&dir) {
                Ok(()) => {
                    let me = Self {
                        file: dir.join(format!("value.{ext}")),
                        dir,
                    };
                    let mut f = create_private_file(&me.file)?;
                    f.write_all(bytes)?;
                    f.sync_all()?;
                    return Ok(me);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no free directory name")))
    }

    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn read(&self) -> std::io::Result<Vec<u8>> {
        fs::read(&self.file)
    }
}

impl Drop for TempValue {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().create(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// The edited bytes: editors add a newline at the end of a file, so one is taken off
/// when the value had none.
pub fn edited_value(before: &[u8], mut after: Vec<u8>) -> Vec<u8> {
    if !before.ends_with(b"\n") && after.ends_with(b"\n") {
        after.pop();
        if !before.ends_with(b"\r") && after.ends_with(b"\r") {
            after.pop();
        }
    }
    after
}

/// Writes the value to a temporary file, runs `command` on it, and reads it back. The
/// file is gone when this returns. The error says what failed, never the value.
pub fn edit_file(base: &Path, job: &EditorJob, command: &[String]) -> Result<Raw, String> {
    let Some((program, args)) = command.split_first() else {
        return Err("no editor is set ($EDITOR)".to_string());
    };
    let temp = TempValue::create(base, job.value.as_bytes(), job.ext)
        .map_err(|e| format!("could not write the temporary file: {e}"))?;
    let status = std::process::Command::new(program)
        .args(args)
        .arg(temp.path())
        .status()
        .map_err(|e| format!("could not start {}: {e}", crate::value::escape(program)))?;
    if !status.success() {
        return Err(format!(
            "{} exited with {status}, so nothing was sent",
            crate::value::escape(program)
        ));
    }
    let after = temp
        .read()
        .map_err(|e| format!("could not read the temporary file back: {e}"))?;
    drop(temp);
    Ok(Raw::new(edited_value(job.value.as_bytes(), after)))
}

/// Runs the job in the terminal: out of the alternate screen and raw mode, with the
/// input thread paused, then back. `out` is the terminal's output. The caller redraws
/// the whole screen afterwards.
pub fn run(job: &EditorJob, out: &mut impl Write) -> Result<Raw, String> {
    use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use crossterm::{cursor, execute};

    let _paused = pause_input();
    let _ = execute!(
        out,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
    let _ = disable_raw_mode();
    let command = editor_command(
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
    );
    let result = edit_file(&std::env::temp_dir(), job, &command);
    let _ = enable_raw_mode();
    let _ = execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        let d = std::env::temp_dir().join(format!("redis-lens-editor-test-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_command_comes_from_visual_then_editor_then_vi() {
        assert_eq!(
            editor_command(Some("code -w"), Some("nano")),
            ["code", "-w"]
        );
        assert_eq!(editor_command(Some("  "), Some("nano")), ["nano"]);
        assert_eq!(editor_command(None, None), ["vi"]);
    }

    #[test]
    fn the_editors_newline_is_taken_off() {
        assert_eq!(edited_value(b"a", b"b\n".to_vec()), b"b");
        assert_eq!(edited_value(b"a", b"b\r\n".to_vec()), b"b");
        assert_eq!(edited_value(b"a\n", b"b\n".to_vec()), b"b\n");
        assert_eq!(edited_value(b"a", b"b".to_vec()), b"b");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempValue::create(&base(), b"s3cr3t", "txt").unwrap();
        let dir = t.dir().to_path_buf();
        let file = t.path().to_path_buf();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&file), 0o600);
        assert_eq!(t.read().unwrap(), b"s3cr3t");
        drop(t);
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_editor_run_reads_the_file_back_and_deletes_it() {
        let job = EditorJob {
            id: 1,
            value: Raw::from("old"),
            ext: "txt",
        };
        let sh = |script: &str| {
            vec![
                "sh".to_string(),
                "-c".to_string(),
                script.to_string(),
                "sh".to_string(),
            ]
        };
        let base = base().join("run");
        fs::create_dir_all(&base).unwrap();
        let got = edit_file(&base, &job, &sh(r#"printf 'new\n' > "$1""#)).unwrap();
        assert_eq!(got.as_bytes(), b"new");
        // A failing editor sends nothing, and the file is gone either way.
        let err = edit_file(&base, &job, &sh("exit 3")).unwrap_err();
        assert!(err.contains("nothing was sent"), "{err}");
        assert!(!err.contains("old"));
        let err = edit_file(&base, &job, &["/no/such/editor".to_string()]).unwrap_err();
        assert!(err.contains("could not start"), "{err}");
        assert_eq!(fs::read_dir(&base).unwrap().count(), 0);
    }
}
