//! The credential of an encrypted database (spec/design/cli.md §3; encryption.md §5): read from a
//! key file, a passphrase file, or a terminal prompt — never a flag's value or an environment
//! variable, which other processes and shell histories can see.

use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Where the credential comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// `--key-file PATH`: 32 raw bytes, or 64 hex digits (surrounding whitespace ignored).
    KeyFile(PathBuf),
    /// `--passphrase-file PATH`: the file's text, less one trailing newline.
    PassphraseFile(PathBuf),
    /// `--passphrase`: prompt on the terminal with echo off.
    Prompt,
}

/// Parse one credential flag: `Ok(None)` when `arg` is not one, `Err` when it repeats or lacks its
/// value.
pub fn parse_flag(
    arg: &str,
    current: &Option<CredentialSource>,
    value: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<Option<CredentialSource>, String> {
    let source = match arg {
        "--key-file" => CredentialSource::KeyFile(PathBuf::from(value("--key-file")?)),
        "--passphrase-file" => {
            CredentialSource::PassphraseFile(PathBuf::from(value("--passphrase-file")?))
        }
        "--passphrase" => CredentialSource::Prompt,
        _ => return Ok(None),
    };
    if current.is_some() {
        return Err("give at most one of --key-file, --passphrase-file, --passphrase".to_string());
    }
    Ok(Some(source))
}

/// Read the credential. `confirm` asks a prompted passphrase twice (a new database).
pub fn load(source: &CredentialSource, confirm: bool) -> Result<jed::Credential, String> {
    match source {
        CredentialSource::KeyFile(path) => {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            parse_key(&bytes).ok_or_else(|| {
                format!(
                    "{}: a key file holds 32 raw bytes or 64 hex digits",
                    path.display()
                )
            })
        }
        CredentialSource::PassphraseFile(path) => {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let text = text
                .strip_suffix("\r\n")
                .or_else(|| text.strip_suffix('\n'))
                .unwrap_or(&text);
            if text.is_empty() {
                return Err(format!("{}: the passphrase is empty", path.display()));
            }
            Ok(jed::Credential::Passphrase(text.to_string()))
        }
        CredentialSource::Prompt => {
            let first = prompt("Passphrase: ")?;
            if confirm && prompt("Confirm passphrase: ")? != first {
                return Err("the passphrases do not match".to_string());
            }
            if first.is_empty() {
                return Err("the passphrase is empty".to_string());
            }
            Ok(jed::Credential::Passphrase(first))
        }
    }
}

/// A key file's 32-byte key.
fn parse_key(bytes: &[u8]) -> Option<jed::Credential> {
    if let Ok(key) = <[u8; 32]>::try_from(bytes) {
        return Some(jed::Credential::Key(key));
    }
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(jed::Credential::Key(key))
}

/// Prompt on the controlling terminal with echo turned off (through `stty`, so the CLI needs no
/// terminal library). Unix only; elsewhere use --passphrase-file.
#[cfg(unix)]
fn prompt(label: &str) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let tty = |e: std::io::Error| format!("cannot prompt for a passphrase: {e}");
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(tty)?;
    let input = std::fs::File::open("/dev/tty").map_err(tty)?;
    let stty = |arg: &str| {
        Command::new("stty")
            .arg(arg)
            .stdin(std::fs::File::open("/dev/tty").map_err(tty)?)
            .stderr(Stdio::null())
            .status()
            .map_err(tty)
    };
    out.write_all(label.as_bytes()).map_err(tty)?;
    out.flush().map_err(tty)?;
    stty("-echo")?;
    let mut line = String::new();
    let read = std::io::BufReader::new(input).read_line(&mut line);
    let _ = stty("echo");
    let _ = out.write_all(b"\n");
    read.map_err(tty)?;
    let line = line
        .strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(&line);
    Ok(line.to_string())
}

#[cfg(not(unix))]
fn prompt(_label: &str) -> Result<String, String> {
    Err("--passphrase needs a terminal; use --passphrase-file".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_files_hold_raw_or_hex_keys() {
        let raw = [7u8; 32];
        assert!(matches!(parse_key(&raw), Some(jed::Credential::Key(k)) if k == raw));
        let hex = format!("{}\n", "07".repeat(32));
        assert!(matches!(parse_key(hex.as_bytes()), Some(jed::Credential::Key(k)) if k == raw));
        assert!(parse_key(b"short").is_none());
        assert!(parse_key("zz".repeat(32).as_bytes()).is_none());
    }

    #[test]
    fn one_credential_flag_at_most() {
        let mut value = |_: &str| Ok("k".to_string());
        let first = parse_flag("--key-file", &None, &mut value).unwrap();
        assert_eq!(first, Some(CredentialSource::KeyFile(PathBuf::from("k"))));
        assert!(parse_flag("--passphrase", &first, &mut value).is_err());
        assert_eq!(parse_flag("-c", &None, &mut value).unwrap(), None);
    }
}
