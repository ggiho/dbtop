//! Puts text on the clipboard.

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// How far the text got.
#[derive(Debug, PartialEq, Eq)]
pub enum Copied {
    /// The system clipboard has it.
    Clipboard,
    /// The terminal was asked to put it there. One that does not allow that says nothing, so this
    /// is no promise.
    Terminal,
}

/// Terminals drop an OSC 52 request longer than this (xterm's limit).
const OSC52_LIMIT: usize = 100_000;

/// The system clipboard where there is a command for it (macOS), else the terminal's, which
/// also reaches the machine one is sitting at when dbtop runs over ssh.
pub fn copy(text: &str) -> Result<Copied, String> {
    if cfg!(target_os = "macos") && pipe_to("pbcopy", &[], text).is_ok() {
        return Ok(Copied::Clipboard);
    }
    let code = osc52(text)?;
    let mut out = io::stdout();
    out.write_all(code.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())?;
    Ok(Copied::Terminal)
}

fn pipe_to(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        // pbcopy reads the locale's encoding, and a shell without one would garble Korean.
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let written = child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(text.as_bytes());
    let status = child.wait()?;
    written?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} failed")))
    }
}

/// The escape sequence that asks a terminal to put `text` on its clipboard. The text travels as
/// base64, so nothing in it can act as an escape of its own.
fn osc52(text: &str) -> Result<String, String> {
    let encoded = base64(text.as_bytes());
    if encoded.len() > OSC52_LIMIT {
        return Err(format!(
            "{} characters are too many for the terminal's clipboard",
            text.chars().count()
        ));
    }
    Ok(format!("\x1b]52;c;{encoded}\x07"))
}

fn base64(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0u32, |group, (i, byte)| {
            group | (u32::from(*byte) << (16 - 8 * i))
        });
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                char::from(DIGITS[((group >> (18 - 6 * i)) & 63) as usize])
            } else {
                '='
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_4648_examples() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("한", "7ZWc"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
    }

    #[test]
    fn the_terminal_is_asked_in_a_form_no_text_can_break_out_of() {
        let code = osc52("SELECT 1\x1b]0;x\x07").unwrap();
        assert!(code.starts_with("\x1b]52;c;") && code.ends_with('\x07'));
        let body = &code[7..code.len() - 1];
        assert!(
            body.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)),
            "{body}"
        );
    }

    #[test]
    fn what_a_terminal_would_drop_is_refused() {
        assert!(osc52(&"x".repeat(60_000)).is_ok());
        let err = osc52(&"x".repeat(80_000)).unwrap_err();
        assert!(err.contains("80000"), "{err}");
    }

    #[test]
    fn text_reaches_the_command_whole() {
        let file = std::env::temp_dir().join(format!("dbtop-clip-{}", std::process::id()));
        // Not the real pbcopy: a stand-in that keeps what it is given.
        let script = format!("cat > {}", file.display());
        pipe_to("sh", &["-c", &script], "SELECT '한글'\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "SELECT '한글'\n");
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn a_command_that_is_missing_or_fails_is_an_error_not_a_hang() {
        assert!(pipe_to("dbtop-no-such-command", &[], "x").is_err());
        assert!(pipe_to("false", &[], "x").is_err());
    }
}
