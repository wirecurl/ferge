//! Getting arbitrary file names safely into POSIX shell scripts.
//!
//! Names on a server may contain anything but `/` and NUL: quotes, `$`,
//! backticks, line breaks. Scripts that ferge generates never paste a name
//! unquoted; these helpers produce the only two forms used.

use std::fmt::Write as _;

/// `text` as one single-quoted POSIX `sh` word. Single quotes end the quoted
/// part, contribute an escaped quote and start a new one: `it's` becomes
/// `'it'\''s'`. Everything else, line breaks included, stays literal.
///
/// Only for scripts run by `sh` itself: login shells such as fish treat
/// backslashes inside single quotes differently.
pub(crate) fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for part in text.split_inclusive('\'') {
        match part.strip_suffix('\'') {
            Some(head) => {
                quoted.push_str(head);
                quoted.push_str("'\\''");
            }
            None => quoted.push_str(part),
        }
    }
    quoted.push('\'');
    quoted
}

/// `text` with every byte outside `[A-Za-z0-9/._-]` written as `\0ooo`, the
/// octal escape understood by `printf %b`. The result is one line of plain
/// ASCII, safe to read with `read -r` and decoded by
/// `x=$(printf '%bx' "$line"); x=${x%x}` (the `x` keeps trailing line breaks).
pub(crate) fn octal_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/._-".contains(&byte) {
            escaped.push(char::from(byte));
        } else {
            let _ = write!(escaped, "\\0{byte:03o}");
        }
    }
    escaped
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    const NASTY: &[&str] = &[
        "/plain/path.txt",
        "/with space/and 'quotes' \"double\"",
        "/dollar $HOME `id` $(id) ${x%x}",
        "/line\nbreak\n",
        "/back\\slash \\0101 %s %b",
        "/-leading dash",
        "/ümlaut 日本 🦀",
        "/glob * ? [a-z] ~ ! # ;&|<>",
    ];

    fn sh(script: &str, input: &str) -> Vec<u8> {
        use std::io::Write as _;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("sh runs");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write");
        let output = child.wait_with_output().expect("sh finishes");
        assert!(output.status.success(), "{script}");
        output.stdout
    }

    #[test]
    fn quoted_words_survive_sh() {
        for text in NASTY {
            let printed = sh(&format!("printf '%s' {}", quote(text)), "");
            assert_eq!(printed, text.as_bytes(), "{text:?}");
        }
    }

    #[test]
    fn octal_escapes_decode_with_printf() {
        for text in NASTY {
            let line = octal_escape(text);
            assert!(!line.contains('\n') && line.is_ascii());
            let printed = sh(
                "IFS= read -r a; a=$(printf '%bx' \"$a\"); printf '%s' \"${a%x}\"",
                &format!("{line}\n"),
            );
            assert_eq!(printed, text.as_bytes(), "{text:?}");
        }
    }
}
