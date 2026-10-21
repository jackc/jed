//! Remote databases over SSH (spec/design/cli.md §3; spec/pageservice/README.md §5.3).
//!
//! - `jed ssh://[user@]host[:port]/path` opens the database at `path` on `host` as a page client:
//!   the CLI runs `ssh [-p port] [user@]host jed pages /path`, and that process's stdin and stdout
//!   carry the page-service connection. The system `ssh` (from `PATH`) does authentication and
//!   encryption; the remote `jed` must be on the remote user's `PATH`.
//! - `jed pages [--read-only] DBFILE` is the remote end: it serves one connection on its stdin and
//!   stdout, and is what an `authorized_keys` `command="jed pages --read-only /srv/app.jed"` names.

/// A parsed `ssh://` database URL.
#[derive(Debug, PartialEq, Eq)]
pub struct SshUrl {
    /// `user@host` or `host`, as `ssh` takes it.
    pub destination: String,
    pub port: Option<u16>,
    /// The database path on the remote host.
    pub path: String,
}

/// Whether a DBFILE argument names a remote database.
pub fn is_ssh_url(arg: &str) -> bool {
    arg.starts_with("ssh://")
}

/// Parse `ssh://[user@]host[:port]/path`. The path is everything from the slash after the host:
/// `ssh://h/srv/app.jed` is `/srv/app.jed`; `ssh://h/~/app.jed` is `~/app.jed`, relative to the
/// remote user's home. An IPv6 host is bracketed (`ssh://[::1]:2222/db.jed`).
pub fn parse_ssh_url(url: &str) -> Result<SshUrl, String> {
    let rest = url
        .strip_prefix("ssh://")
        .ok_or_else(|| format!("not an ssh:// URL: {url}"))?;
    let slash = rest
        .find('/')
        .ok_or_else(|| format!("bad ssh:// URL: {url} (expected ssh://[user@]host[:port]/path)"))?;
    let (authority, path) = rest.split_at(slash);
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((user, hostport)) => (Some(user), hostport),
        None => (None, authority),
    };
    let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .ok_or_else(|| format!("bad ssh:// URL: {url} (unclosed '[')"))?;
        match after {
            "" => (host, None),
            _ => match after.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None => return Err(format!("bad ssh:// URL: {url}")),
            },
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (hostport, None),
        }
    };
    if host.is_empty() || user == Some("") {
        return Err(format!(
            "bad ssh:// URL: {url} (expected ssh://[user@]host[:port]/path)"
        ));
    }
    let port = match port {
        None => None,
        Some(p) => Some(
            p.parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("bad port in ssh:// URL: {url}"))?,
        ),
    };
    let path = match path.strip_prefix("/~") {
        Some(home) if home.is_empty() || home.starts_with('/') => format!("~{home}"),
        _ => path.to_string(),
    };
    if path == "/" || path == "~" || path == "~/" {
        return Err(format!("ssh:// URL names no database file: {url}"));
    }
    let destination = match user {
        Some(user) => format!("{user}@{host}"),
        None => host.to_string(),
    };
    Ok(SshUrl {
        destination,
        port,
        path,
    })
}

/// Quote `s` as one word for the remote POSIX shell (`ssh` joins its command arguments into one
/// string the remote shell splits again). A leading `~/` stays unquoted so the shell expands it.
fn shell_word(s: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    match s.strip_prefix("~/") {
        Some(rest) if !rest.is_empty() => format!("~/{}", quote(rest)),
        _ => quote(s),
    }
}

/// The transport command for `url`: `ssh [-p port] -- destination 'jed pages path'`.
pub fn ssh_command(url: &SshUrl) -> Vec<String> {
    let mut command = vec!["ssh".to_string()];
    if let Some(port) = url.port {
        command.push("-p".to_string());
        command.push(port.to_string());
    }
    command.push("--".to_string());
    command.push(url.destination.clone());
    command.push(format!("jed pages {}", shell_word(&url.path)));
    command
}

/// Open the remote database `url` names as a page client over `ssh` (`name` is the URL as given).
/// An encrypted database unlocks with `credential` on this side; the remote end relays ciphertext.
pub fn open(
    url: &SshUrl,
    name: &str,
    read_only: bool,
    credential: Option<jed::Credential>,
) -> jed::Result<jed::Database> {
    #[cfg(unix)]
    {
        jed::Database::open_remote(
            ssh_command(url),
            name,
            jed::OpenOptions {
                read_only,
                credential,
                ..jed::OpenOptions::default()
            },
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (url, name, read_only, credential);
        Err(jed::EngineError::new(
            jed::SqlState::FeatureNotSupported,
            "ssh:// databases are not supported on this platform",
        ))
    }
}

pub const PAGES_USAGE: &str = "\
usage: jed pages [--read-only] [--key-file FILE | --passphrase-file FILE] DBFILE

Serve one page-service connection for DBFILE on stdin/stdout: the remote end of
`jed ssh://host/DBFILE`, which runs it over ssh. It relays to the process that owns
DBFILE, or owns DBFILE itself (handing ownership to an application that asks).
  --read-only             refuse writes on the connection, whatever the client asks
  --key-file FILE         the key of an encrypted DBFILE, needed only to own it (a relay
  --passphrase-file FILE  passes ciphertext through); without one, owning it fails 28P02";

/// `jed pages …` (cli.md §3). Exit status 0 when the connection ended, 1 on a usage error or when
/// the database could not be served (the client was told why on the stream).
pub fn pages(argv: &[String]) -> u8 {
    let mut read_only = false;
    let mut path = None;
    let mut credential = None;
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match crate::credential::parse_flag(arg, &credential, &mut value) {
            // stdin carries the connection, so there is no terminal to prompt on.
            Ok(Some(crate::credential::CredentialSource::Prompt)) => {
                eprintln!("jed pages: --passphrase cannot prompt here; use --passphrase-file");
                return 1;
            }
            Ok(Some(source)) => {
                credential = Some(source);
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("jed pages: {e}\n\n{PAGES_USAGE}");
                return 1;
            }
        }
        match arg.as_str() {
            "--read-only" => read_only = true,
            "-h" | "--help" => {
                eprintln!("{PAGES_USAGE}");
                return 0;
            }
            s if s.starts_with('-') => {
                eprintln!("jed pages: unknown flag: {s}\n\n{PAGES_USAGE}");
                return 1;
            }
            _ if path.is_some() => {
                eprintln!("jed pages: unexpected extra argument: {arg}\n\n{PAGES_USAGE}");
                return 1;
            }
            _ => path = Some(std::path::PathBuf::from(arg)),
        }
    }
    let Some(path) = path else {
        eprintln!("jed pages: missing DBFILE\n\n{PAGES_USAGE}");
        return 1;
    };
    let credential = match &credential {
        Some(source) => match crate::credential::load(source, false) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("jed pages: {e}");
                return 1;
            }
        },
        None => None,
    };
    // stdout carries the connection: nothing else may be written to it.
    #[cfg(unix)]
    let served = jed::Database::serve_page_stream(
        &path,
        jed::OpenOptions {
            credential,
            ..jed::OpenOptions::default()
        },
        read_only,
        Box::new(std::io::stdin()),
        Box::new(std::io::stdout()),
    );
    #[cfg(not(unix))]
    let served: jed::Result<()> = {
        let _ = (&path, read_only, credential);
        Err(jed::EngineError::new(
            jed::SqlState::FeatureNotSupported,
            "jed pages is not supported on this platform",
        ))
    };
    match served {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("jed pages: ERROR {}: {}", e.code(), e.message);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(destination: &str, port: Option<u16>, path: &str) -> SshUrl {
        SshUrl {
            destination: destination.to_string(),
            port,
            path: path.to_string(),
        }
    }

    #[test]
    fn parses_ssh_urls() {
        assert_eq!(
            parse_ssh_url("ssh://db1/srv/app.jed").unwrap(),
            url("db1", None, "/srv/app.jed")
        );
        assert_eq!(
            parse_ssh_url("ssh://app@db1:2222/srv/app.jed").unwrap(),
            url("app@db1", Some(2222), "/srv/app.jed")
        );
        assert_eq!(
            parse_ssh_url("ssh://db1/~/app.jed").unwrap(),
            url("db1", None, "~/app.jed")
        );
        assert_eq!(
            parse_ssh_url("ssh://[::1]:22/x.jed").unwrap(),
            url("::1", Some(22), "/x.jed")
        );
        assert_eq!(
            parse_ssh_url("ssh://[fe80::1]/x.jed").unwrap(),
            url("fe80::1", None, "/x.jed")
        );
    }

    #[test]
    fn rejects_bad_ssh_urls() {
        for bad in [
            "ssh://db1",
            "ssh:///x.jed",
            "ssh://db1:0/x.jed",
            "ssh://db1:port/x.jed",
            "ssh://@db1/x.jed",
            "ssh://db1/",
            "ssh://db1/~/",
            "ssh://[::1/x.jed",
        ] {
            assert!(parse_ssh_url(bad).is_err(), "{bad} parsed");
        }
    }

    #[test]
    fn builds_the_ssh_command() {
        assert_eq!(
            ssh_command(&url("app@db1", Some(2222), "/srv/my app's.jed")),
            vec![
                "ssh",
                "-p",
                "2222",
                "--",
                "app@db1",
                r"jed pages '/srv/my app'\''s.jed'"
            ]
        );
        assert_eq!(
            ssh_command(&url("db1", None, "~/app.jed")),
            vec!["ssh", "--", "db1", "jed pages ~/'app.jed'"]
        );
    }
}
