//! Host key verification against OpenSSH known_hosts files.
//!
//! Implements the lookup rules of `sshd(8)`: comma-separated patterns with `*`,
//! `?` and `!` negation, `[host]:port` for non-standard ports, hashed host names
//! (`|1|salt|hash`) and the `@revoked` marker. `@cert-authority` lines are
//! ignored because host certificates are not supported yet.

use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use hmac::{Hmac, KeyInit as _, Mac as _};
use russh::keys::ssh_key::known_hosts::{Entry, HostPatterns, Marker};
use russh::keys::{HashAlg, PublicKey};
use sha1::Sha1;

use crate::error::HostKeyError;

/// Result of looking up a host key.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The key is recorded for this host.
    Trusted,
    /// Nothing is recorded for this key type.
    Unknown { other_types_known: bool },
    /// A different key of the same type is recorded.
    Changed { file: PathBuf, line: usize },
    /// The key is explicitly revoked.
    Revoked,
}

/// The known_hosts files consulted for one connection.
#[derive(Clone, Debug)]
pub(crate) struct KnownHosts {
    /// Writable files; new keys go into the first one.
    pub user_files: Vec<PathBuf>,
    /// Read-only system files.
    pub global_files: Vec<PathBuf>,
}

/// The name a host is recorded under: `host` for port 22, `[host]:port` otherwise.
pub(crate) fn host_key_name(hostname: &str, port: u16) -> String {
    let hostname = hostname.to_ascii_lowercase();
    if port == super::config::DEFAULT_PORT {
        hostname
    } else {
        format!("[{hostname}]:{port}")
    }
}

/// OpenSSH-style SHA-256 fingerprint, e.g. `SHA256:…`.
pub(crate) fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

impl KnownHosts {
    /// Looks `key` up for `name` (see [`host_key_name`]).
    pub(crate) async fn check(&self, name: &str, key: &PublicKey) -> Result<Verdict, HostKeyError> {
        let mut other_types_known = false;
        let mut changed: Option<(PathBuf, usize)> = None;
        let mut trusted = false;

        for file in self.user_files.iter().chain(&self.global_files) {
            let Some(text) = read_if_exists(file).await? else {
                continue;
            };
            for (line, entry) in parse_entries(&text, file) {
                if !entry_matches(&entry, name) {
                    continue;
                }
                let same_key = entry.public_key().key_data() == key.key_data();
                match entry.marker() {
                    Some(Marker::Revoked) if same_key => return Ok(Verdict::Revoked),
                    Some(_) => {}
                    None if same_key => trusted = true,
                    None if entry.public_key().algorithm() == key.algorithm() => {
                        changed.get_or_insert_with(|| (file.clone(), line));
                    }
                    None => other_types_known = true,
                }
            }
        }

        // Revocation was handled above. A matching key wins over stale entries of
        // the same type (a host may legitimately list several keys).
        Ok(if trusted {
            Verdict::Trusted
        } else if let Some((file, line)) = changed {
            Verdict::Changed { file, line }
        } else {
            Verdict::Unknown { other_types_known }
        })
    }

    /// Appends `key` for `name` to the first user file.
    pub(crate) async fn remember(&self, name: &str, key: &PublicKey) -> Result<(), HostKeyError> {
        let Some(file) = self.user_files.first().cloned() else {
            return Err(HostKeyError::Save {
                path: PathBuf::from("known_hosts"),
                source: "no writable known_hosts file is configured".into(),
            });
        };
        let line = format!(
            "{name} {}\n",
            key.to_openssh().map_err(|error| HostKeyError::Save {
                path: file.clone(),
                source: error.into(),
            })?
        );
        let target = file.clone();
        tokio::task::spawn_blocking(move || append_line(&target, &line))
            .await
            .map_err(|error| HostKeyError::Save {
                path: file.clone(),
                source: error.into(),
            })?
            .map_err(|error| HostKeyError::Save {
                path: file,
                source: error.into(),
            })
    }
}

async fn read_if_exists(path: &Path) -> Result<Option<String>, HostKeyError> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(HostKeyError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Parses all valid entries with their 1-based line numbers. Malformed lines and
/// unsupported key types are skipped, as OpenSSH does.
fn parse_entries<'a>(text: &'a str, file: &'a Path) -> impl Iterator<Item = (usize, Entry)> + 'a {
    text.lines().enumerate().filter_map(move |(index, raw)| {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        // OpenSSH separates fields by any whitespace; the parser only by spaces.
        let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
        match Entry::from_str(&normalized) {
            Ok(entry) => Some((index + 1, entry)),
            Err(error) => {
                tracing::debug!(file = %file.display(), line = index + 1, %error, "skipping known_hosts line");
                None
            }
        }
    })
}

fn entry_matches(entry: &Entry, name: &str) -> bool {
    match entry.host_patterns() {
        HostPatterns::HashedName { salt, hash } => {
            let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(salt) else {
                return false;
            };
            mac.update(name.as_bytes());
            mac.verify_slice(hash).is_ok()
        }
        HostPatterns::Patterns(patterns) => {
            let mut matched = false;
            for pattern in patterns {
                if let Some(negated) = pattern.strip_prefix('!') {
                    if crate::glob::matches(negated, name, true) {
                        return false;
                    }
                } else if crate::glob::matches(pattern, name, true) {
                    matched = true;
                }
            }
            matched
        }
    }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    let mut needs_newline = false;
    if file.seek(SeekFrom::End(-1)).is_ok() {
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)?;
        needs_newline = last[0] != b'\n';
    }
    let mut buffer = String::with_capacity(line.len() + 1);
    if needs_newline {
        buffer.push('\n');
    }
    buffer.push_str(line);
    // A single write keeps concurrent appends from interleaving partial lines.
    file.write_all(buffer.as_bytes())?;
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use base64ct::{Base64, Encoding as _};

    use super::*;

    const ED25519_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJzaHLNcaADvZ+bfJbe+3qF3Z3UkZs0s717aac8buuEz";
    const ED25519_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJQowyCPaSFPVdlK/mf7Ah7dYVdmZHCTOhtWieCfkD6x";
    const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBNUsVjiS1PLgS1YWnHCI+sUgOEtq87YTEa323a47Jnk3stqw5Kw3WpnU3NMfaPMOgrsWVu4mJ2YXh7rdZJd08Y8=";

    fn key(text: &str) -> PublicKey {
        PublicKey::from_openssh(text).expect("valid key")
    }

    async fn check(contents: &str, name: &str, server_key: &str) -> Verdict {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("known_hosts");
        std::fs::write(&file, contents).expect("write");
        let known = KnownHosts {
            user_files: vec![file],
            global_files: vec![dir.path().join("missing")],
        };
        known.check(name, &key(server_key)).await.expect("check")
    }

    fn hashed(name: &str) -> String {
        let salt = [7_u8; 20];
        let mut mac = Hmac::<Sha1>::new_from_slice(&salt).expect("hmac");
        mac.update(name.as_bytes());
        let hash = mac.finalize().into_bytes();
        format!(
            "|1|{}|{}",
            Base64::encode_string(&salt),
            Base64::encode_string(&hash)
        )
    }

    #[tokio::test]
    async fn trusted_unknown_and_changed() {
        let file = format!("# comment\n\nexample.org,10.0.0.1 {ED25519_A}\n");
        assert_eq!(
            check(&file, "example.org", ED25519_A).await,
            Verdict::Trusted
        );
        assert_eq!(check(&file, "10.0.0.1", ED25519_A).await, Verdict::Trusted);
        assert_eq!(
            check(&file, "other.org", ED25519_A).await,
            Verdict::Unknown {
                other_types_known: false
            }
        );
        assert!(matches!(
            check(&file, "example.org", ED25519_B).await,
            Verdict::Changed { line: 3, .. }
        ));
        assert_eq!(
            check(&file, "example.org", ECDSA).await,
            Verdict::Unknown {
                other_types_known: true
            }
        );
    }

    #[tokio::test]
    async fn ports_wildcards_and_negation() {
        let file = format!("[example.org]:2222 {ED25519_A}\n*.lan,!evil.lan {ED25519_B}\n");
        assert_eq!(
            check(&file, "[example.org]:2222", ED25519_A).await,
            Verdict::Trusted
        );
        assert_eq!(
            check(&file, "example.org", ED25519_A).await,
            Verdict::Unknown {
                other_types_known: false
            }
        );
        assert_eq!(check(&file, "nas.lan", ED25519_B).await, Verdict::Trusted);
        assert_eq!(
            check(&file, "evil.lan", ED25519_B).await,
            Verdict::Unknown {
                other_types_known: false
            }
        );
        assert_eq!(check(&file, "NAS.LAN", ED25519_B).await, Verdict::Trusted);
    }

    #[tokio::test]
    async fn hashed_names_and_markers() {
        let file = format!("{} {ED25519_A}\n", hashed("[h.example]:2222"));
        assert_eq!(
            check(&file, "[h.example]:2222", ED25519_A).await,
            Verdict::Trusted
        );
        assert!(matches!(
            check(&file, "[h.example]:2222", ED25519_B).await,
            Verdict::Changed { .. }
        ));
        assert_eq!(
            check(&file, "h.example", ED25519_A).await,
            Verdict::Unknown {
                other_types_known: false
            }
        );

        let file = format!("h.example {ED25519_A}\n@revoked * {ED25519_A}\n");
        assert_eq!(check(&file, "h.example", ED25519_A).await, Verdict::Revoked);
        let file = format!("@cert-authority * {ED25519_A}\n");
        assert_eq!(
            check(&file, "h.example", ED25519_A).await,
            Verdict::Unknown {
                other_types_known: false
            }
        );
    }

    #[tokio::test]
    async fn malformed_lines_are_skipped() {
        let file = format!(
            "garbage\nexample.org ssh-ed25519 notbase64\nexample.org\t{ED25519_A} comment\n"
        );
        assert_eq!(
            check(&file, "example.org", ED25519_A).await,
            Verdict::Trusted
        );
    }

    #[tokio::test]
    async fn remember_appends_a_matching_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("nested/known_hosts");
        let known = KnownHosts {
            user_files: vec![file.clone()],
            global_files: Vec::new(),
        };
        let name = host_key_name("Example.ORG", 2222);
        assert_eq!(name, "[example.org]:2222");
        known
            .remember(&name, &key(ED25519_A))
            .await
            .expect("remember");
        std::fs::write(
            &file,
            format!(
                "{}no-newline",
                std::fs::read_to_string(&file).expect("read")
            ),
        )
        .expect("write");
        known
            .remember("other", &key(ED25519_B))
            .await
            .expect("remember");
        assert_eq!(
            known.check(&name, &key(ED25519_A)).await.expect("check"),
            Verdict::Trusted
        );
        assert_eq!(
            known.check("other", &key(ED25519_B)).await.expect("check"),
            Verdict::Trusted
        );
    }

    #[test]
    fn fingerprints_use_sha256() {
        assert!(fingerprint(&key(ED25519_A)).starts_with("SHA256:"));
    }
}
