//! The fields that describe a server, as Quick Connect and the favorite
//! editor have them: a protocol, the server, port, user and folder. The
//! server field also takes whole addresses (`user@host:port`, `sftp://…`,
//! `ftp://…`); an address with a scheme picks the protocol itself.

use ferge_core::{Destination, FtpTarget, FtpTls, RemotePath, SftpTarget};

/// The protocols to choose from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    Sftp,
    /// FTP with explicit TLS (or none, if the user agrees).
    Ftp,
    /// FTP with implicit TLS.
    Ftps,
}

impl Protocol {
    pub(crate) const CHOICES: [(Self, &'static str); 3] = [
        (Self::Sftp, "SFTP"),
        (Self::Ftp, "FTP"),
        (Self::Ftps, "FTPS"),
    ];

    /// The protocol of `destination`; `None` for this computer.
    pub(crate) fn of(destination: &Destination) -> Option<Self> {
        match destination {
            Destination::Local { .. } => None,
            Destination::Sftp(_) => Some(Self::Sftp),
            Destination::Ftp(target) => Some(match target.tls {
                FtpTls::Explicit => Self::Ftp,
                FtpTls::Implicit => Self::Ftps,
            }),
        }
    }

    pub(crate) fn host_hint(self) -> &'static str {
        match self {
            Self::Sftp => "example.org, an address or an ssh_config alias",
            Self::Ftp | Self::Ftps => "example.org or an address",
        }
    }

    pub(crate) fn port_hint(self) -> &'static str {
        match self {
            Self::Sftp => "22",
            Self::Ftp => "21",
            Self::Ftps => "990",
        }
    }

    pub(crate) fn user_hint(self) -> &'static str {
        match self {
            Self::Sftp => "From ssh_config, or your login name",
            Self::Ftp | Self::Ftps => "anonymous if empty",
        }
    }

    /// What the choice means, shown below it.
    pub(crate) fn note(self) -> &'static str {
        match self {
            Self::Sftp => "Files over SSH, with your keys and ssh_config.",
            Self::Ftp => "Encrypted with TLS; ferge asks before going without.",
            Self::Ftps => "FTP over TLS from the start, usually on port 990.",
        }
    }
}

/// The parts of a server address, whatever the protocol.
struct Parts {
    host: String,
    port: Option<u16>,
    user: Option<String>,
    path: Option<RemotePath>,
}

/// The server the fields describe. The other fields fill in what `server`
/// leaves out and must agree with what it says.
pub(crate) fn destination(
    protocol: Protocol,
    server: &str,
    port: &str,
    user: &str,
    path: &str,
) -> Result<Destination, String> {
    if server.is_empty() {
        return Err("Enter a server to connect to.".into());
    }
    let invalid = || format!("“{server}” is not a valid server name.");
    let named_scheme = server.contains("://");
    let (protocol, mut parts) = match Destination::parse(server) {
        Ok(Destination::Ftp(target)) => (
            Protocol::of(&Destination::Ftp(target.clone())).unwrap_or(protocol),
            Parts {
                host: target.host,
                port: target.port,
                user: target.user,
                path: target.path,
            },
        ),
        Ok(Destination::Sftp(target)) => (
            if named_scheme {
                Protocol::Sftp
            } else {
                protocol
            },
            Parts {
                host: target.host,
                port: target.port,
                user: target.user,
                path: target.path,
            },
        ),
        // `Destination::parse` reads `local` and absolute paths as this
        // computer; here they can only be meant as a host name.
        Ok(Destination::Local { .. }) => (
            protocol,
            Parts {
                host: SftpTarget::new(server).map_err(|_| invalid())?.host,
                port: None,
                user: None,
                path: None,
            },
        ),
        Err(_) => return Err(invalid()),
    };
    fill_in(&mut parts, port, user, path)?;
    Ok(match protocol {
        Protocol::Sftp => {
            let mut target = SftpTarget::new(&parts.host).map_err(|_| invalid())?;
            target.port = parts.port;
            target.user = parts.user;
            target.path = parts.path;
            Destination::Sftp(target)
        }
        Protocol::Ftp | Protocol::Ftps => {
            let tls = if protocol == Protocol::Ftp {
                FtpTls::Explicit
            } else {
                FtpTls::Implicit
            };
            let mut target = FtpTarget::new(&parts.host, tls).map_err(|_| invalid())?;
            target.port = parts.port;
            target.user = parts.user;
            target.path = parts.path;
            Destination::Ftp(target)
        }
    })
}

/// Fills in what the server field left out from the other fields, which
/// must agree with what it says.
fn fill_in(parts: &mut Parts, port: &str, user: &str, path: &str) -> Result<(), String> {
    if !port.is_empty() {
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("“{port}” is not a valid port."))?;
        match parts.port {
            Some(given) if given != port => {
                return Err(format!(
                    "The server says port {given}, the port field {port}."
                ));
            }
            _ => parts.port = Some(port),
        }
    }
    if !user.is_empty() {
        if user
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '@')
        {
            return Err(format!("“{user}” is not a valid user name."));
        }
        match &parts.user {
            Some(given) if given != user => {
                return Err(format!(
                    "The server says user “{given}”, the user field “{user}”."
                ));
            }
            _ => parts.user = Some(user.to_owned()),
        }
    }
    if !path.is_empty() {
        let path = RemotePath::parse(path)
            .map_err(|_| "The folder must be an absolute path like /srv/www.")?;
        match &parts.path {
            Some(given) if *given != path => {
                return Err(format!(
                    "The address says folder {given}, the folder field {path}."
                ));
            }
            _ => parts.path = Some(path),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Shown = (
        Protocol,
        String,
        Option<u16>,
        Option<String>,
        Option<String>,
    );

    fn parts(destination: &Destination) -> Shown {
        let protocol = Protocol::of(destination).unwrap();
        match destination {
            Destination::Sftp(target) => (
                protocol,
                target.host.clone(),
                target.port,
                target.user.clone(),
                target.path.as_ref().map(ToString::to_string),
            ),
            Destination::Ftp(target) => (
                protocol,
                target.host.clone(),
                target.port,
                target.user.clone(),
                target.path.as_ref().map(ToString::to_string),
            ),
            Destination::Local { .. } => unreachable!(),
        }
    }

    fn sftp(server: &str, port: &str, user: &str, path: &str) -> Result<Shown, String> {
        destination(Protocol::Sftp, server, port, user, path).map(|d| parts(&d))
    }

    fn shown(
        protocol: Protocol,
        host: &str,
        port: Option<u16>,
        user: Option<&str>,
        path: Option<&str>,
    ) -> Shown {
        (
            protocol,
            host.to_owned(),
            port,
            user.map(str::to_owned),
            path.map(str::to_owned),
        )
    }

    #[test]
    fn reads_addresses_in_the_server_field() {
        assert_eq!(
            sftp("root@prod.example.org", "", "", ""),
            Ok(shown(
                Protocol::Sftp,
                "prod.example.org",
                None,
                Some("root"),
                None
            ))
        );
        assert_eq!(
            sftp("root@prod.example.org:2222", "", "", ""),
            Ok(shown(
                Protocol::Sftp,
                "prod.example.org",
                Some(2222),
                Some("root"),
                None
            ))
        );
        assert_eq!(
            sftp("sftp://me@[::1]:2200/srv/www", "", "", ""),
            Ok(shown(
                Protocol::Sftp,
                "::1",
                Some(2200),
                Some("me"),
                Some("/srv/www")
            ))
        );
        assert_eq!(
            sftp("::1", "", "", ""),
            Ok(shown(Protocol::Sftp, "::1", None, None, None))
        );
        // A host that happens to be called `local`.
        assert_eq!(
            sftp("local", "22", "", ""),
            Ok(shown(Protocol::Sftp, "local", Some(22), None, None))
        );
    }

    #[test]
    fn an_address_with_a_scheme_picks_the_protocol() {
        assert_eq!(
            sftp("ftps://me@files.example.org", "", "", ""),
            Ok(shown(
                Protocol::Ftps,
                "files.example.org",
                None,
                Some("me"),
                None
            ))
        );
        assert_eq!(
            destination(Protocol::Ftp, "sftp://host", "", "", "").map(|d| parts(&d)),
            Ok(shown(Protocol::Sftp, "host", None, None, None))
        );
        // Without a scheme, the choice counts.
        assert_eq!(
            destination(Protocol::Ftp, "me@host:2121", "", "", "/pub").map(|d| parts(&d)),
            Ok(shown(
                Protocol::Ftp,
                "host",
                Some(2121),
                Some("me"),
                Some("/pub")
            ))
        );
    }

    #[test]
    fn fields_fill_in_and_must_agree() {
        assert_eq!(
            sftp("root@host", "2222", "root", "/tmp"),
            Ok(shown(
                Protocol::Sftp,
                "host",
                Some(2222),
                Some("root"),
                Some("/tmp")
            ))
        );
        assert_eq!(
            sftp("host", "", "alice", ""),
            Ok(shown(Protocol::Sftp, "host", None, Some("alice"), None))
        );
        assert!(sftp("root@host", "", "alice", "").is_err());
        assert!(sftp("host:2222", "22", "", "").is_err());
        assert!(sftp("sftp://host/a", "", "", "/b").is_err());
    }

    #[test]
    fn rejects_bad_input() {
        for (server, port, user, path) in [
            ("", "", "", ""),
            ("/tmp", "", "", ""),
            ("@host", "", "", ""),
            ("a b", "", "", ""),
            ("host:0", "", "", ""),
            ("host", "x", "", ""),
            ("host", "", "a@b", ""),
            ("host", "", "", "relative"),
        ] {
            for protocol in [Protocol::Sftp, Protocol::Ftp] {
                assert!(
                    destination(protocol, server, port, user, path).is_err(),
                    "{server:?} {port:?} {user:?} {path:?}"
                );
            }
        }
    }
}
