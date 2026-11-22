//! Where to connect to: the local file system or an SFTP server, optionally at a
//! given directory.
//!
//! A [`Destination`] has a canonical textual form, its *address*, that is
//! accepted by [`Destination::parse`] again. Addresses are what the address
//! field shows and what the connection history records.

use std::fmt::{self, Write as _};

use crate::error::{Error, Result};
use crate::path::RemotePath;
use crate::remote::Remote;
use crate::remote::sftp::SftpOptions;

/// The local file system or an SFTP server.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Destination {
    /// The local file system, at `path` or the home directory.
    Local { path: Option<RemotePath> },
    /// An SFTP server.
    Sftp(SftpTarget),
}

/// An SFTP server and an optional start directory. Unset values come from
/// `ssh_config` or the defaults.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SftpTarget {
    /// Host name, address or `ssh_config` alias.
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    /// Start directory; the login directory if unset.
    pub path: Option<RemotePath>,
}

impl SftpTarget {
    /// A target for `host` with everything else left to the configuration.
    ///
    /// The host must be non-empty and contain no whitespace, control
    /// characters, `@`, `/` or brackets (except around an IPv6 address).
    pub fn new(host: &str) -> Result<Self> {
        let host = host.trim();
        let bare = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(host);
        let valid = !bare.is_empty()
            && !bare.starts_with('-')
            && bare
                .chars()
                .all(|c| !(c.is_whitespace() || c.is_control() || "@/[]".contains(c)));
        if !valid {
            return Err(Error::Config(format!("invalid host {host:?}")));
        }
        Ok(Self {
            host: bare.to_owned(),
            port: None,
            user: None,
            path: None,
        })
    }

    /// Connection options for this target with all other settings at their
    /// defaults.
    pub fn options(&self) -> SftpOptions {
        let mut options = SftpOptions::new(self.host.clone());
        options.port = self.port;
        options.user.clone_from(&self.user);
        options
    }

    /// `user@host:port` with the parts that are set, for labels.
    pub fn authority(&self) -> String {
        let mut authority = String::new();
        if let Some(user) = &self.user {
            authority.push_str(user);
            authority.push('@');
        }
        if self.host.contains(':') {
            authority.push('[');
            authority.push_str(&self.host);
            authority.push(']');
        } else {
            authority.push_str(&self.host);
        }
        if let Some(port) = self.port {
            authority.push(':');
            authority.push_str(&port.to_string());
        }
        authority
    }
}

impl Destination {
    /// Whether `remote` is connected to this file system or server as given
    /// here (host as asked for, and user and port where set); the start
    /// directory is not compared.
    pub fn is_served_by(&self, remote: &dyn Remote) -> bool {
        match self {
            Self::Local { .. } => remote.is_local(),
            Self::Sftp(target) => remote.server_side().is_some_and(|server| {
                let endpoint = server.endpoint();
                endpoint.alias.eq_ignore_ascii_case(&target.host)
                    && target
                        .user
                        .as_ref()
                        .is_none_or(|user| *user == endpoint.user)
                    && target.port.is_none_or(|port| port == endpoint.port)
            }),
        }
    }

    /// Parses what a user typed. Accepted forms:
    /// - `local`, or an absolute local path such as `/tmp`
    /// - `sftp://[user@]host[:port][/path]`, the path may be percent-encoded
    /// - `[user@]host[:port]`, which includes plain `ssh_config` aliases
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            return Err(Error::Config(
                "enter a host, an sftp:// address or a local path".into(),
            ));
        }
        if input.eq_ignore_ascii_case("local") {
            return Ok(Self::Local { path: None });
        }
        if input.starts_with('/') {
            return Ok(Self::Local {
                path: Some(RemotePath::parse(input)?),
            });
        }
        let (authority, path) = match input.strip_prefix("sftp://") {
            Some(rest) => match rest.find('/') {
                Some(slash) => {
                    let path = RemotePath::parse(&percent_decode(&rest[slash..])?)?;
                    (&rest[..slash], Some(path))
                }
                None => (rest, None),
            },
            None => (input, None),
        };
        let options = SftpOptions::parse_destination(authority)?;
        let mut target = SftpTarget::new(&options.host)?;
        target.port = options.port;
        target.user = options.user;
        target.path = path;
        Ok(Self::Sftp(target))
    }

    /// The canonical address, accepted by [`Destination::parse`].
    pub fn address(&self) -> String {
        self.to_string()
    }

    /// The start directory, if one is set.
    pub fn path(&self) -> Option<&RemotePath> {
        match self {
            Self::Local { path } => path.as_ref(),
            Self::Sftp(target) => target.path.as_ref(),
        }
    }

    /// The same server or file system at `path`.
    #[must_use]
    pub fn with_path(&self, path: Option<RemotePath>) -> Self {
        match self {
            Self::Local { .. } => Self::Local { path },
            Self::Sftp(target) => Self::Sftp(SftpTarget {
                path,
                ..target.clone()
            }),
        }
    }

    /// A short name for lists and tab titles: the host or "Local files".
    pub fn title(&self) -> String {
        match self {
            Self::Local { .. } => "Local files".to_owned(),
            Self::Sftp(target) => target.host.clone(),
        }
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { path: None } => f.write_str("local"),
            Self::Local { path: Some(path) } => f.write_str(path.as_str()),
            Self::Sftp(target) => {
                write!(f, "sftp://{}", target.authority())?;
                if let Some(path) = &target.path {
                    f.write_str(&percent_encode_path(path.as_str()))?;
                }
                Ok(())
            }
        }
    }
}

/// Encodes the characters that would change the meaning of an address:
/// `%`, whitespace, control characters, `?` and `#`.
fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for c in path.chars() {
        if c == '%' || c == '?' || c == '#' || c.is_whitespace() || c.is_control() {
            let mut buffer = [0; 4];
            for byte in c.encode_utf8(&mut buffer).bytes() {
                let _ = write!(encoded, "%{byte:02X}");
            }
        } else {
            encoded.push(c);
        }
    }
    encoded
}

/// Decodes `%XX` escapes; the result must be valid UTF-8.
fn percent_decode(text: &str) -> Result<String> {
    let invalid = || Error::Config(format!("invalid percent-encoding in {text:?}"));
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
    fn this_computer_serves_local_places_only() {
        let local = crate::remote::local::LocalRemote::new();
        assert!(Destination::Local { path: None }.is_served_by(&local));
        assert!(
            !Destination::parse("sftp://user@host")
                .unwrap()
                .is_served_by(&local)
        );
    }

    fn sftp(input: &str) -> SftpTarget {
        match Destination::parse(input) {
            Ok(Destination::Sftp(target)) => target,
            other => panic!("{input:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn local_destinations() {
        assert_eq!(
            Destination::parse("local").unwrap(),
            Destination::Local { path: None }
        );
        assert_eq!(
            Destination::parse(" /tmp/../var ").unwrap().address(),
            "/var"
        );
    }

    #[test]
    fn sftp_destinations() {
        let target = sftp("sftp://ferge@127.0.0.1:2222/home/ferge/with%20space");
        assert_eq!(target.user.as_deref(), Some("ferge"));
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, Some(2222));
        assert_eq!(
            target.path.as_ref().map(RemotePath::as_str),
            Some("/home/ferge/with space")
        );
        let alias = sftp("myalias");
        assert_eq!(alias.host, "myalias");
        assert!(alias.path.is_none() && alias.user.is_none() && alias.port.is_none());
        assert_eq!(sftp("[::1]:22").host, "::1");
    }

    #[test]
    fn addresses_round_trip() {
        for address in [
            "local",
            "/srv/data",
            "myalias",
            "sftp://host",
            "sftp://me@host:2222/a b/100%25/x?y#z",
            "sftp://[::1]:2200/",
            "sftp://ünï@hôst/ü",
        ] {
            let parsed = Destination::parse(address).unwrap();
            let canonical = parsed.address();
            assert_eq!(
                Destination::parse(&canonical).unwrap(),
                parsed,
                "{address:?} → {canonical:?}"
            );
        }
        assert_eq!(
            Destination::parse("me@host:2222").unwrap().address(),
            "sftp://me@host:2222"
        );
        assert_eq!(
            Destination::parse("sftp://h/a b/100%25").unwrap().address(),
            "sftp://h/a%20b/100%25"
        );
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
            "-oProxyCommand=x",
            "host name",
        ] {
            assert!(Destination::parse(bad).is_err(), "{bad:?}");
        }
        for bad in ["", "a b", "a/b", "u@h", "-x", "[]"] {
            assert!(SftpTarget::new(bad).is_err(), "{bad:?}");
        }
    }
}
