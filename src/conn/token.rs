//! Running a profile's token command ([`crate::model::Auth::Command`]).
//!
//! Owner: connections. The command is the program and its arguments from the config
//! file, run directly (never through a shell) with standard input closed and standard
//! error discarded. Its standard output, minus one trailing newline, is the token. The
//! token is kept in a [`SecretString`] and is never written anywhere: errors name the
//! program and its exit status, never what it printed. Standard error is never shown
//! either, because tools can echo tokens there.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use secrecy::SecretString;

/// How long the command may run before it is killed.
pub const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

/// The most standard output a token command may print. Cloud tokens are a few KiB at
/// most (an Entra ID access token is about 2 KiB, an AWS IAM auth token about 1 KiB);
/// anything longer is not a token, and reading stops there.
pub const TOKEN_MAX_BYTES: usize = 64 * 1024;

/// How often [`run_with`] checks whether the command has exited.
const POLL: Duration = Duration::from_millis(10);

/// Why a token command gave no token. None of the texts hold the token, the command's
/// output or its arguments: only the program, which comes from the config file.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error(
        "the profile has no token command; add it again with `redis-lens conn add <name> --url <url> -- <command>`"
    )]
    Empty,
    #[error("could not run the token command {program}: {reason}")]
    Spawn { program: String, reason: String },
    #[error(
        "the token command {program} did not finish within {secs} s and was stopped",
        secs = TOKEN_TIMEOUT.as_secs()
    )]
    Timeout { program: String },
    #[error(
        "the token command {program} failed ({status}); run it yourself to see why (redis-lens never shows its output)"
    )]
    Failed { program: String, status: String },
    #[error("the token command {program} printed no token")]
    NoOutput { program: String },
    #[error(
        "the token command {program} printed more than {kib} KiB, which is not a token",
        kib = TOKEN_MAX_BYTES / 1024
    )]
    TooLong { program: String },
    #[error("the token command {program} printed text that is not UTF-8")]
    NotUtf8 { program: String },
}

/// Run `argv` and return the token it prints, within [`TOKEN_TIMEOUT`] and
/// [`TOKEN_MAX_BYTES`]. Blocks the calling thread.
pub fn run_token_command(argv: &[String]) -> Result<SecretString, TokenError> {
    run_with(argv, TOKEN_TIMEOUT, TOKEN_MAX_BYTES)
}

/// [`run_token_command`] with the limits as arguments, for tests.
fn run_with(argv: &[String], timeout: Duration, max: usize) -> Result<SecretString, TokenError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(TokenError::Empty);
    };
    if program.trim().is_empty() {
        return Err(TokenError::Empty);
    }
    let name = || program.clone();
    let started = Instant::now();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| TokenError::Spawn {
            program: name(),
            reason: e.to_string(),
        })?;

    // Read on a helper thread so the timeout holds even while the command writes
    // nothing. At most `max + 1` bytes are read: one more than allowed says "too long".
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = channel();
    let reader = std::thread::Builder::new()
        .name("token-command".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let r = (&mut stdout).take(max as u64 + 1).read_to_end(&mut buf);
            let _ = tx.send(r.map(|_| buf));
        });
    if reader.is_err() {
        stop(&mut child);
        return Err(TokenError::Spawn {
            program: name(),
            reason: "could not start a thread to read its output".into(),
        });
    }

    let left = |now: Instant| timeout.saturating_sub(now.saturating_duration_since(started));
    let out = match rx.recv_timeout(left(Instant::now())) {
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => {
            stop(&mut child);
            return Err(TokenError::Spawn {
                program: name(),
                reason: format!("could not read its output: {}", e.kind()),
            });
        }
        Err(RecvTimeoutError::Timeout) => {
            stop(&mut child);
            return Err(TokenError::Timeout { program: name() });
        }
        Err(RecvTimeoutError::Disconnected) => {
            stop(&mut child);
            return Err(TokenError::Spawn {
                program: name(),
                reason: "its output could not be read".into(),
            });
        }
    };
    if out.len() > max {
        stop(&mut child);
        return Err(TokenError::TooLong { program: name() });
    }

    // Output is closed; wait for the exit status within what is left of the timeout.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if left(Instant::now()).is_zero() => {
                stop(&mut child);
                return Err(TokenError::Timeout { program: name() });
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                stop(&mut child);
                return Err(TokenError::Spawn {
                    program: name(),
                    reason: format!("could not wait for it: {}", e.kind()),
                });
            }
        }
    };
    if !status.success() {
        return Err(TokenError::Failed {
            program: name(),
            status: status_text(status),
        });
    }

    let mut token = String::from_utf8(out).map_err(|_| TokenError::NotUtf8 { program: name() })?;
    // One trailing newline (`\n` or `\r\n`), as `echo`, `az … -o tsv` and most tools
    // print; anything else is part of the token. Truncating keeps the allocation, so
    // no copy of the token is left behind.
    if token.ends_with('\n') {
        token.pop();
        if token.ends_with('\r') {
            token.pop();
        }
    }
    if token.is_empty() {
        return Err(TokenError::NoOutput { program: name() });
    }
    Ok(SecretString::from(token))
}

/// Kill the command and reap it. A command that already exited is not an error.
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// `exit status 1`, or `killed by signal 9` on Unix.
fn status_text(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit status {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("killed by signal {sig}");
        }
    }
    status.to_string()
}

/// The command as it would be typed, for `conn add` to show before saving it. Arguments
/// with anything other than letters, digits and `-_./:=@,+%` are single-quoted. For
/// display only: the command is never run through a shell.
pub fn display_command(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let plain = !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c));
            if plain {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    /// A shell is only used here, to write test commands; redis-lens never runs one.
    fn sh(script: &str) -> Vec<String> {
        argv(&["/bin/sh", "-c", script])
    }

    #[test]
    fn prints_a_token() {
        let t = run_token_command(&argv(&["echo", "secret-token-123"])).unwrap();
        assert_eq!(t.expose_secret(), "secret-token-123");
        // Only one trailing newline goes; the rest is the token.
        let t = run_token_command(&argv(&["printf", "tok\r\n"])).unwrap();
        assert_eq!(t.expose_secret(), "tok");
        let t = run_token_command(&argv(&["printf", "tok"])).unwrap();
        assert_eq!(t.expose_secret(), "tok");
        let t = run_token_command(&argv(&["printf", " tok \n\n"])).unwrap();
        assert_eq!(t.expose_secret(), " tok \n");
    }

    #[test]
    fn arguments_are_not_shell_words() {
        // No shell: `$HOME` and `;` reach the program as they are.
        let t = run_token_command(&argv(&["echo", "$HOME;", "x"])).unwrap();
        assert_eq!(t.expose_secret(), "$HOME; x");
    }

    #[test]
    fn stdin_is_closed_and_stderr_is_dropped() {
        // `cat` with stdin closed prints nothing, and finishes at once.
        let e = run_token_command(&argv(&["cat"])).unwrap_err();
        assert!(matches!(e, TokenError::NoOutput { .. }), "{e:?}");
        let t = run_token_command(&sh("echo leaked-on-stderr >&2; echo tok")).unwrap();
        assert_eq!(t.expose_secret(), "tok");
    }

    #[test]
    fn timeout_kills_the_command() {
        let started = Instant::now();
        let e = run_with(
            &sh("echo partial-token; exec sleep 30"),
            Duration::from_millis(300),
            1024,
        )
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(e, TokenError::Timeout { .. }), "{e:?}");
        let text = e.to_string();
        assert_eq!(
            text,
            "the token command /bin/sh did not finish within 10 s and was stopped"
        );
        assert!(!text.contains("partial-token"));

        // Silent until the timeout, too.
        let e = run_with(&argv(&["sleep", "30"]), Duration::from_millis(200), 1024).unwrap_err();
        assert!(matches!(e, TokenError::Timeout { .. }), "{e:?}");
    }

    #[test]
    fn non_zero_exit_names_the_program_and_status_only() {
        let e = run_token_command(&sh("echo secret-token-123; echo oops-secret >&2; exit 3"))
            .unwrap_err();
        let text = e.to_string();
        assert_eq!(
            text,
            "the token command /bin/sh failed (exit status 3); run it yourself to see why \
             (redis-lens never shows its output)"
        );
        for s in [text, format!("{e:?}")] {
            assert!(!s.contains("secret-token-123"), "{s}");
            assert!(!s.contains("oops-secret"), "{s}");
            // The arguments are not repeated either.
            assert!(!s.contains("exit 3"), "{s}");
        }
        let e = run_token_command(&argv(&["false"])).unwrap_err();
        assert!(e.to_string().contains("(exit status 1)"), "{e}");
    }

    #[test]
    fn killed_by_a_signal() {
        let e = run_token_command(&sh("kill -9 $$")).unwrap_err();
        assert!(e.to_string().contains("killed by signal 9"), "{e}");
    }

    #[test]
    fn empty_output_is_an_error() {
        for cmd in [argv(&["true"]), argv(&["echo"]), argv(&["printf", "\r\n"])] {
            let e = run_token_command(&cmd).unwrap_err();
            assert!(matches!(e, TokenError::NoOutput { .. }), "{cmd:?}: {e:?}");
            assert!(e.to_string().ends_with("printed no token"), "{e}");
        }
    }

    #[test]
    fn huge_output_is_capped() {
        // Far more than the cap, and endless: reading stops at the cap and the command
        // is killed.
        let started = Instant::now();
        let e = run_token_command(&argv(&["yes", "secret-token-123"])).unwrap_err();
        assert!(matches!(e, TokenError::TooLong { .. }), "{e:?}");
        assert!(started.elapsed() < TOKEN_TIMEOUT);
        assert!(!e.to_string().contains("secret-token-123"));
        assert!(e.to_string().contains("64 KiB"), "{e}");
        // Exactly at the cap is fine.
        let t = run_with(
            &sh("head -c 100 /dev/zero | tr '\\0' x"),
            TOKEN_TIMEOUT,
            100,
        )
        .unwrap();
        assert_eq!(t.expose_secret().len(), 100);
        let e = run_with(
            &sh("head -c 101 /dev/zero | tr '\\0' x"),
            TOKEN_TIMEOUT,
            100,
        )
        .unwrap_err();
        assert!(matches!(e, TokenError::TooLong { .. }), "{e:?}");
    }

    #[test]
    fn not_utf8_is_an_error() {
        let e = run_token_command(&argv(&["printf", "\\377\\376"])).unwrap_err();
        assert!(matches!(e, TokenError::NotUtf8 { .. }), "{e:?}");
    }

    #[test]
    fn missing_program_and_empty_command() {
        let e =
            run_token_command(&argv(&["/nonexistent/redis-lens-token-tool", "--x"])).unwrap_err();
        let text = e.to_string();
        assert!(
            text.starts_with(
                "could not run the token command /nonexistent/redis-lens-token-tool: "
            ),
            "{text}"
        );
        assert!(!text.contains("--x"), "{text}");
        assert!(matches!(run_token_command(&[]), Err(TokenError::Empty)));
        assert!(matches!(
            run_token_command(&argv(&[" "])),
            Err(TokenError::Empty)
        ));
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        assert_eq!(
            display_command(&argv(&[
                "az",
                "account",
                "get-access-token",
                "--resource",
                "https://redis.azure.com",
                "--query",
                "accessToken",
                "-o",
                "tsv",
            ])),
            "az account get-access-token --resource https://redis.azure.com --query accessToken -o tsv"
        );
        assert_eq!(
            display_command(&argv(&["tool", "a b", "it's", ""])),
            r"tool 'a b' 'it'\''s' ''"
        );
    }
}
