//! Parsing of what the user types into the address field or passes on the
//! command line.

use ferry_core::RemotePath;
use ferry_core::remote::sftp::SftpOptions;

/// Where to connect to.
#[derive(Debug, Clone)]
pub(crate) enum Destination {
    /// The local file system, optionally at a given directory.
    Local(Option<RemotePath>),
    /// An SFTP server, optionally at a given directory.
    Sftp {
        options: Box<SftpOptions>,
        path: Option<RemotePath>,
    },
}

impl Destination {
    /// Accepted forms:
    /// - `local` or an absolute local path such as `/tmp`
    /// - `sftp://[user@]host[:port][/path]` (path may be percent-encoded)
    /// - `[user@]host[:port]` or an `ssh_config` alias
    pub(crate) fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("enter a host, an sftp:// address or a local path".into());
        }
        if input.eq_ignore_ascii_case("local") {
            return Ok(Self::Local(None));
        }
        if input.starts_with('/') {
            let path = RemotePath::parse(input).map_err(|error| error.to_string())?;
            return Ok(Self::Local(Some(path)));
        }
        let (authority, path) = match input.strip_prefix("sftp://") {
            Some(rest) => match rest.find('/') {
                Some(slash) => {
                    let decoded = percent_decode(&rest[slash..])?;
                    let path = RemotePath::parse(&decoded).map_err(|error| error.to_string())?;
                    (&rest[..slash], Some(path))
                }
                None => (rest, None),
            },
            None => (input, None),
        };
        let options =
            SftpOptions::parse_destination(authority).map_err(|error| error.to_string())?;
        Ok(Self::Sftp {
            options: Box::new(options),
            path,
        })
    }
}

/// Decodes `%XX` escapes; the result must be valid UTF-8.
fn percent_decode(text: &str) -> Result<String, String> {
    let invalid = || format!("invalid percent-encoding in {text:?}");
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or_else(invalid)?;
            let hex = std::str::from_utf8(hex).map_err(|_| invalid())?;
            decoded.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_destinations() {
        assert!(matches!(
            Destination::parse("local"),
            Ok(Destination::Local(None))
        ));
        match Destination::parse(" /tmp/../var ") {
            Ok(Destination::Local(Some(path))) => assert_eq!(path.as_str(), "/var"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sftp_destinations() {
        match Destination::parse("sftp://ferry@127.0.0.1:2222/home/ferry/with%20space") {
            Ok(Destination::Sftp { options, path }) => {
                assert_eq!(options.user.as_deref(), Some("ferry"));
                assert_eq!(options.host, "127.0.0.1");
                assert_eq!(options.port, Some(2222));
                assert_eq!(
                    path.map(|p| p.to_string()).as_deref(),
                    Some("/home/ferry/with space")
                );
            }
            other => panic!("{other:?}"),
        }
        match Destination::parse("myalias") {
            Ok(Destination::Sftp { options, path }) => {
                assert_eq!(options.host, "myalias");
                assert!(path.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invalid_destinations() {
        for bad in [
            "",
            "  ",
            "sftp://",
            "sftp://host/%zz",
            "sftp://host/%ff",
            "user@",
        ] {
            assert!(Destination::parse(bad).is_err(), "{bad:?}");
        }
    }
}
