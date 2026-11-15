//! The file the browser is opened with ([`super::open_in_browser`]): the sign-in URL
//! holds the session token, so it never goes on a command line. Instead it is written
//! into a small HTML page that sends the browser on (a meta refresh, and a plain link
//! for a browser that does not follow it), in a directory of its own that only this
//! user can read (unix: the directory `0700`, the file `0600`, names from `getrandom`;
//! Windows: under the user's own temp directory). The opener gets that file's path.
//! The file is deleted after [`KEEP`] and when the dashboard stops ([`remove_all`]).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

/// How long the file is kept for the browser to read it.
pub const KEEP: Duration = Duration::from_secs(30);

/// The files written and not deleted yet.
static WRITTEN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn written() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    WRITTEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// A random name: 16 bytes as hex.
fn random_name() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|_| std::io::Error::other("no random bytes"))?;
    Ok(super::auth::hex(&b))
}

/// Where the directory goes: the user's runtime directory when there is one (Linux),
/// else the temp directory (per user on macOS and Windows).
fn base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// The page that sends the browser to `url`. `url` is ours (`http://127.0.0.1:...`,
/// hex), but it is escaped for an attribute all the same.
pub fn page(url: &str) -> String {
    let u: String = url
        .chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '"' => "&quot;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            c => c.to_string(),
        })
        .collect();
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <meta http-equiv=\"refresh\" content=\"0;url={u}\">\
         <title>redis-lens</title></head>\
         <body><p><a href=\"{u}\">Open the redis-lens dashboard</a></p></body></html>\n"
    )
}

/// Writes the page for `url` into a new private directory under `base` and returns
/// the file's path.
pub fn write_in(base: &Path, url: &str) -> std::io::Result<PathBuf> {
    let dir = base.join(format!("redis-lens-{}", random_name()?));
    // Only Unix sets a mode on it, so on Windows `mut` is unused.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut db = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        db.mode(0o700);
    }
    db.create(&dir)?;
    let path = dir.join(format!("open-{}.html", random_name()?));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let written_ok = o
        .open(&path)
        .and_then(|mut f| f.write_all(page(url).as_bytes()));
    if let Err(e) = written_ok {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        return Err(e);
    }
    written().push(path.clone());
    Ok(path)
}

/// [`write_in`] the usual place.
pub fn write(url: &str) -> std::io::Result<PathBuf> {
    write_in(&base(), url)
}

/// The command that opens `file` in the default browser: the path is its one argument
/// (after `url.dll,FileProtocolHandler` on Windows).
pub fn command(file: &Path) -> Command {
    if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(file);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(file);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(file);
        c
    }
}

/// Deletes `file` and its directory.
pub fn remove(file: &Path) {
    let _ = std::fs::remove_file(file);
    if let Some(dir) = file.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    written().retain(|p| p != file);
}

/// Deletes `file` after `wait`, on a thread of its own.
pub fn remove_after(file: PathBuf, wait: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        remove(&file);
    });
}

/// Deletes every file written and not deleted yet: the dashboard stops.
pub fn remove_all() {
    let all: Vec<PathBuf> = std::mem::take(&mut *written());
    for f in all {
        remove(&f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "redis-lens-opener-{tag}-{}-{}",
            std::process::id(),
            random_name().unwrap()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const URL: &str = "http://127.0.0.1:7379/#token=00ff00ff";

    #[test]
    fn the_file_is_private_sends_the_browser_on_and_is_the_only_argument() {
        let base = tmp("write");
        let f = write_in(&base, URL).unwrap();
        let body = std::fs::read_to_string(&f).unwrap();
        assert!(body.contains(
            "http-equiv=\"refresh\" content=\"0;url=http://127.0.0.1:7379/#token=00ff00ff\""
        ));
        assert!(body.contains("<a href=\"http://127.0.0.1:7379/#token=00ff00ff\">"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
            let dir_mode = std::fs::metadata(f.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
            assert_eq!(dir_mode, 0o700);
        }
        // The command names the file, never the URL or the token.
        let c = command(&f);
        let args: Vec<String> = std::iter::once(c.get_program())
            .chain(c.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == &f.to_string_lossy()), "{args:?}");
        for a in &args {
            assert!(
                !a.contains("token") && !a.contains("00ff00ff") && !a.contains("http"),
                "{a}"
            );
        }
        remove(&f);
        assert!(!f.exists());
        assert!(!f.parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_file_goes_after_a_while_and_when_the_dashboard_stops() {
        let base = tmp("remove");
        let a = write_in(&base, URL).unwrap();
        remove_after(a.clone(), Duration::from_millis(100));
        let t0 = std::time::Instant::now();
        while a.exists() {
            assert!(t0.elapsed() < Duration::from_secs(5), "not deleted");
            std::thread::sleep(Duration::from_millis(20));
        }
        let b = write_in(&base, URL).unwrap();
        assert!(b.exists());
        remove_all();
        assert!(!b.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_url_is_escaped_in_the_page() {
        let p = page("http://x/\"><script>");
        assert!(!p.contains("<script>"), "{p}");
        assert!(p.contains("&quot;&gt;&lt;script&gt;"));
    }
}
