//! Hidden password prompting, shared by `set-password` (issue #51) and
//! `bootstrap-household` (issue #139).
//!
//! On a TTY the password is read twice with a hidden prompt (no echo) and the two
//! entries must match. Off a TTY (piped stdin, for scripting) only the first line
//! of stdin is used; no confirmation is required and nothing is echoed. Either way
//! the password is never echoed and the shared policy
//! ([`buywise_core::password::validate_new_password`] — a non-empty password)
//! rejects an empty one.

use std::io::{BufRead, IsTerminal};

use anyhow::{bail, Context, Result};
use buywise_core::password::validate_new_password;

/// Read a new password without echoing it, then validate it against the shared
/// policy. See the module docs for the TTY vs piped behaviour.
pub fn read_password() -> Result<String> {
    if std::io::stdin().is_terminal() {
        let first = rpassword::prompt_password("New password: ")
            .context("reading password from terminal")?;
        let confirm = rpassword::prompt_password("Confirm password: ")
            .context("reading password confirmation from terminal")?;
        validate_password(first, Some(confirm))
    } else {
        let line = read_password_from_stdin(&mut std::io::stdin().lock())?;
        validate_password(line, None)
    }
}

/// Validate (and return) a password drawn from either input path.
///
/// Requires the two entries to match when a `confirmation` is supplied (the
/// interactive path), then applies [`validate_new_password`]. Pure over plain
/// strings so the rejection branches can be unit-tested without a TTY.
fn validate_password(password: String, confirmation: Option<String>) -> Result<String> {
    if let Some(confirm) = confirmation {
        if password != confirm {
            bail!("passwords did not match");
        }
    }
    validate_new_password(&password).map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(password)
}

/// Read a password from a non-TTY reader (the scripting path).
///
/// Reads a single line, trimming a trailing `\n`/`\r\n` (so
/// `echo secret | buywise-cli …` works) but preserving any other whitespace the
/// caller intended as part of the password. Only the first line is consumed —
/// the reader is not drained to EOF, so a long piped stream is not slurped into
/// memory. Factored out so it can be unit tested without a real stdin.
fn read_password_from_stdin(reader: &mut impl BufRead) -> Result<String> {
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .context("reading password from stdin")?;
    let line = buf.strip_suffix('\n').unwrap_or(&buf);
    Ok(line.strip_suffix('\r').unwrap_or(line).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_password_rejects_empty() {
        let err = validate_password(String::new(), None).expect_err("empty must be rejected");
        assert!(err.to_string().contains("must not be empty"), "got: {err}");

        // Empty is rejected even when it "matches" an empty confirmation.
        let err = validate_password(String::new(), Some(String::new()))
            .expect_err("empty must be rejected");
        assert!(err.to_string().contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn validate_password_rejects_confirmation_mismatch() {
        let err = validate_password("hunter2".into(), Some("hunter3".into()))
            .expect_err("mismatch must be rejected");
        assert!(err.to_string().contains("did not match"), "got: {err}");
    }

    #[test]
    fn validate_password_accepts_matching_nonempty() {
        // Interactive path: matching confirmation.
        assert_eq!(
            validate_password("hunter2".into(), Some("hunter2".into())).unwrap(),
            "hunter2"
        );
        // Scripting path: no confirmation required.
        assert_eq!(
            validate_password("hunter2".into(), None).unwrap(),
            "hunter2"
        );
    }

    #[test]
    fn stdin_reader_takes_first_line_trimming_newline() {
        let mut input = "hunter2\n".as_bytes();
        assert_eq!(read_password_from_stdin(&mut input).unwrap(), "hunter2");

        let mut crlf = "hunter2\r\n".as_bytes();
        assert_eq!(read_password_from_stdin(&mut crlf).unwrap(), "hunter2");

        // Only the first line is used; a trailing second line is ignored.
        let mut multi = "secret\nignored\n".as_bytes();
        assert_eq!(read_password_from_stdin(&mut multi).unwrap(), "secret");

        // No trailing newline still works.
        let mut bare = "secret".as_bytes();
        assert_eq!(read_password_from_stdin(&mut bare).unwrap(), "secret");
    }
}
