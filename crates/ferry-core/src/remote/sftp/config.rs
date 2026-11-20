//! Connection options and their resolution against `~/.ssh/config`.
//!
//! Parsing is delegated to `russh-config`. Its parser only understands a subset
//! of `ssh_config(5)`, so the file is first rewritten into that subset:
//! `Key=Value` and tab separators are normalized, `Include` is inlined, options
//! before the first `Host` become a `Host *` section, and `Match` blocks (which
//! russh-config would wrongly merge into the preceding `Host`) are disabled.
//! Tokens such as `%h` are expanded afterwards.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use secrecy::SecretString;

use crate::error::{Error, Result};

/// Default TCP port of SSH.
pub(crate) const DEFAULT_PORT: u16 = 22;

/// Maximum nesting of `Include` directives, as in OpenSSH.
const MAX_INCLUDE_DEPTH: u8 = 16;

/// Which `ssh_config` file to consult.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SshConfigSource {
    /// `~/.ssh/config`, if it exists.
    #[default]
    User,
    /// A specific file.
    File(PathBuf),
    /// Ignore all configuration files.
    Disabled,
}

/// Which known_hosts files to consult.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum KnownHostsSource {
    /// `UserKnownHostsFile` from the configuration, falling back to
    /// `~/.ssh/known_hosts`, plus the system-wide files.
    #[default]
    Default,
    /// Only this file. Accepted keys are recorded in it.
    File(PathBuf),
}

/// What to connect to and how. Explicit values win over `ssh_config`.
#[derive(Clone, Debug)]
pub struct SftpOptions {
    /// Host name, address or `ssh_config` alias.
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    /// Private keys to try before those from the configuration.
    pub identity_files: Vec<PathBuf>,
    /// Whether to fall back to `~/.ssh/id_ed25519`, `id_ecdsa` and `id_rsa` when
    /// neither the options nor the configuration name a key.
    pub use_default_identities: bool,
    /// A password to use without asking.
    pub password: Option<SecretString>,
    /// Whether to offer keys held by the SSH agent (`SSH_AUTH_SOCK`).
    pub use_agent: bool,
    pub ssh_config: SshConfigSource,
    pub known_hosts: KnownHostsSource,
    /// Upper bound for establishing the TCP connection.
    pub connect_timeout: Duration,
}

impl SftpOptions {
    /// Options for `host` with all defaults.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
            user: None,
            identity_files: Vec::new(),
            use_default_identities: true,
            password: None,
            use_agent: true,
            ssh_config: SshConfigSource::default(),
            known_hosts: KnownHostsSource::default(),
            connect_timeout: Duration::from_secs(15),
        }
    }

    /// Parses `[user@]host[:port]`. IPv6 addresses need brackets when a port is given.
    pub fn parse_destination(destination: &str) -> Result<Self> {
        let invalid = || Error::Config(format!("invalid destination {destination:?}"));
        let (user, rest) = match destination.rsplit_once('@') {
            Some((user, rest)) if !user.is_empty() => (Some(user.to_owned()), rest),
            Some(_) => return Err(invalid()),
            None => (None, destination),
        };
        let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
            let (host, tail) = bracketed.split_once(']').ok_or_else(invalid)?;
            let port = match tail {
                "" => None,
                tail => Some(tail.strip_prefix(':').ok_or_else(invalid)?),
            };
            (host, port)
        } else if rest.matches(':').count() == 1 {
            let (host, port) = rest.split_once(':').ok_or_else(invalid)?;
            (host, Some(port))
        } else {
            (rest, None)
        };
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(invalid());
        }
        let port = port
            .map(|port| {
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(invalid)
            })
            .transpose()?;
        let mut options = Self::new(host);
        options.user = user;
        options.port = port;
        Ok(options)
    }
}

/// Fully resolved connection parameters.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedTarget {
    /// What the user typed; used in labels and as `%n`.
    pub alias: String,
    /// The real host name to connect to and to look up in known_hosts.
    pub hostname: String,
    pub port: u16,
    pub user: String,
    /// Keys to try, in order.
    pub identity_files: Vec<PathBuf>,
    /// Whether `identity_files` was configured (rather than OpenSSH's defaults).
    pub identities_configured: bool,
    /// Command whose stdin/stdout replace the TCP connection.
    pub proxy_command: Option<String>,
    /// `HostKeyAlias`: the name to look up and record the host key under.
    pub host_key_alias: Option<String>,
    /// known_hosts files that may be written to, in order.
    pub user_known_hosts: Vec<PathBuf>,
    /// Read-only, system-wide known_hosts files.
    pub global_known_hosts: Vec<PathBuf>,
}

impl ResolvedTarget {
    /// `user@host` with the port when it is not the default.
    pub(crate) fn label(&self) -> String {
        if self.port == DEFAULT_PORT {
            format!("{}@{}", self.user, self.alias)
        } else {
            format!("{}@{}:{}", self.user, self.alias, self.port)
        }
    }
}

/// Resolves `options` against the SSH configuration. Performs blocking file I/O;
/// call it from a blocking context.
pub(crate) fn resolve(options: &SftpOptions) -> Result<ResolvedTarget> {
    let home = dirs::home_dir();
    let normalized = normalized_config(&options.ssh_config, home.as_deref())?;
    let parsed = russh_config::parse(&normalized, &options.host)
        .map_err(|error| Error::Config(error.to_string()))?;
    let mut target = build_target(options, &parsed, home.as_deref())?;
    // Options russh-config does not know are looked up directly.
    target.host_key_alias =
        lookup_option(&normalized, &options.host, "hostkeyalias").filter(|alias| !alias.is_empty());
    if let Some(alias) = &target.host_key_alias {
        validate_name("HostKeyAlias", alias, is_plain_char)?;
    }
    Ok(target)
}

/// The first value of `keyword` (lower-case) that applies to `host` in a
/// normalized configuration, following OpenSSH's "first match wins" rule.
fn lookup_option(normalized: &str, host: &str, keyword: &str) -> Option<String> {
    let mut applies = false;
    for line in normalized.lines() {
        if let Some(patterns) = line.strip_prefix("Host ") {
            applies = section_matches(patterns, host);
            continue;
        }
        if !applies {
            continue;
        }
        let (key, value) = split_keyword(line);
        if key.eq_ignore_ascii_case(keyword) {
            return Some(strip_quotes(value).to_owned());
        }
    }
    None
}

/// `Host` line semantics: any positive pattern matches and no negated one does.
fn section_matches(patterns: &str, host: &str) -> bool {
    let mut matched = false;
    for pattern in patterns.split_whitespace() {
        if let Some(negated) = pattern.strip_prefix('!') {
            if crate::glob::matches(negated, host, true) {
                return false;
            }
        } else if crate::glob::matches(pattern, host, true) {
            matched = true;
        }
    }
    matched
}

/// Host aliases defined in the SSH configuration, in file order, for quick
/// connect lists. Wildcard and negated patterns are left out. Performs
/// blocking file I/O.
pub fn configured_hosts(source: &SshConfigSource) -> Result<Vec<String>> {
    let normalized = normalized_config(source, dirs::home_dir().as_deref())?;
    let mut seen = std::collections::HashSet::new();
    Ok(normalized
        .lines()
        .filter_map(|line| line.strip_prefix("Host "))
        .flat_map(str::split_whitespace)
        .filter(|pattern| !pattern.contains(['*', '?', '!']))
        .filter(|pattern| seen.insert(pattern.to_string()))
        .map(str::to_owned)
        .collect())
}

/// Reads the configuration and rewrites it for russh-config (see [`preprocess`]).
fn normalized_config(source: &SshConfigSource, home: Option<&Path>) -> Result<String> {
    let (text, base_dir) = match source {
        SshConfigSource::Disabled => return Ok(String::new()),
        SshConfigSource::User => {
            let Some(home) = home else {
                return Ok(String::new());
            };
            let ssh_dir = home.join(".ssh");
            (read_optional(&ssh_dir.join("config"))?, ssh_dir)
        }
        SshConfigSource::File(path) => {
            let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
                context: format!("could not read {}", path.display()),
                source,
            })?;
            (
                Some(text),
                path.parent().map(Path::to_path_buf).unwrap_or_default(),
            )
        }
    };
    let mut out = String::new();
    if let Some(text) = text {
        preprocess(&text, &base_dir, home, 0, &mut out);
    }
    Ok(out)
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            context: format!("could not read {}", path.display()),
            source,
        }),
    }
}

fn build_target(
    options: &SftpOptions,
    parsed: &russh_config::Config,
    home: Option<&Path>,
) -> Result<ResolvedTarget> {
    let config = &parsed.host_config;
    if let Some(jump) = config.proxy_jump.as_deref()
        && !jump.eq_ignore_ascii_case("none")
    {
        return Err(Error::Unsupported(
            "ProxyJump (use ProxyCommand instead for now)",
        ));
    }

    let alias = options.host.clone();
    let user = options.user.clone().unwrap_or_else(|| parsed.user());
    let port = options.port.unwrap_or_else(|| parsed.port());
    if port == 0 {
        return Err(Error::Config("port 0 is not valid".into()));
    }
    validate_name("host", &alias, is_plain_char)?;
    validate_name("user", &user, is_plain_char)?;
    let local_user = local_user_name();
    let home_text = home
        .map(|home| home.to_string_lossy().into_owned())
        .unwrap_or_default();

    // `HostName` may itself contain `%h` (the alias) and `%%`.
    let hostname_tokens = Tokens {
        alias: &alias,
        hostname: &alias,
        port,
        remote_user: &user,
        local_user: &local_user,
        home: &home_text,
    };
    let hostname = config.hostname.as_deref().map_or_else(
        || alias.clone(),
        |name| expand_tokens(name, &hostname_tokens),
    );
    let tokens = Tokens {
        hostname: &hostname,
        ..hostname_tokens
    };

    let (identity_files, identities_configured) = identity_files(options, config, &tokens, home);

    validate_name("host name", &hostname, is_plain_char)?;
    let proxy_command = match config
        .proxy_command
        .as_deref()
        .filter(|command| !command.eq_ignore_ascii_case("none"))
    {
        Some(command) => {
            // Expanded values end up in `sh -c`; refuse anything a shell would
            // interpret (see CVE-2023-51385).
            validate_name("host", &alias, is_shell_safe_char)?;
            validate_name("host name", &hostname, is_shell_safe_char)?;
            validate_name("user", &user, is_shell_safe_char)?;
            Some(expand_tokens(command, &tokens))
        }
        None => None,
    };

    let (user_known_hosts, global_known_hosts) =
        known_hosts_files(&options.known_hosts, config, &tokens, home);

    Ok(ResolvedTarget {
        alias,
        hostname,
        port,
        user,
        identity_files,
        identities_configured,
        proxy_command,
        host_key_alias: None,
        user_known_hosts,
        global_known_hosts,
    })
}

/// Keys to try, and whether any were named explicitly (options or configuration).
fn identity_files(
    options: &SftpOptions,
    config: &russh_config::HostConfig,
    tokens: &Tokens<'_>,
    home: Option<&Path>,
) -> (Vec<PathBuf>, bool) {
    let configured: Vec<PathBuf> = config
        .identity_file
        .iter()
        .flatten()
        .map(|path| expand_path(&path.to_string_lossy(), tokens, home))
        .collect();
    let identities_configured = !(options.identity_files.is_empty() && configured.is_empty());
    let mut files: Vec<PathBuf> = options.identity_files.clone();
    files.extend(configured);
    if !identities_configured
        && options.use_default_identities
        && let Some(home) = home
    {
        let ssh_dir = home.join(".ssh");
        files.extend(
            ["id_ed25519", "id_ecdsa", "id_rsa"]
                .iter()
                .map(|name| ssh_dir.join(name)),
        );
    }
    dedup_preserving_order(&mut files);
    (files, identities_configured)
}

/// Writable user files and read-only system files, in lookup order.
fn known_hosts_files(
    source: &KnownHostsSource,
    config: &russh_config::HostConfig,
    tokens: &Tokens<'_>,
    home: Option<&Path>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    match source {
        KnownHostsSource::File(path) => (vec![path.clone()], Vec::new()),
        KnownHostsSource::Default => {
            let user_files = match &config.user_known_hosts_file {
                // russh-config stores the raw value as one path; OpenSSH allows a
                // whitespace-separated list.
                Some(value) => {
                    let value = value.to_string_lossy();
                    if value.eq_ignore_ascii_case("none") {
                        Vec::new()
                    } else {
                        value
                            .split_whitespace()
                            .map(|file| expand_path(file, tokens, home))
                            .collect()
                    }
                }
                None => home
                    .map(|home| {
                        let ssh_dir = home.join(".ssh");
                        vec![ssh_dir.join("known_hosts"), ssh_dir.join("known_hosts2")]
                    })
                    .unwrap_or_default(),
            };
            let global_files = vec![
                PathBuf::from("/etc/ssh/ssh_known_hosts"),
                PathBuf::from("/etc/ssh/ssh_known_hosts2"),
            ];
            (user_files, global_files)
        }
    }
}

/// Rejects empty values, a leading `-` (option injection) and characters for which
/// `allowed` returns false.
fn validate_name(what: &str, value: &str, allowed: fn(char) -> bool) -> Result<()> {
    if value.is_empty() || value.starts_with('-') || !value.chars().all(allowed) {
        return Err(Error::Config(format!("invalid {what} {value:?}")));
    }
    Ok(())
}

/// Anything but whitespace and control characters; these would corrupt
/// known_hosts lines and log output.
fn is_plain_char(c: char) -> bool {
    !(c.is_whitespace() || c.is_control())
}

/// Characters that are inert in a POSIX shell word.
fn is_shell_safe_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%' | '@' | '+' | '/')
}

fn local_user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

fn dedup_preserving_order(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

/// Values for `ssh_config` percent tokens.
#[derive(Clone, Copy)]
struct Tokens<'a> {
    alias: &'a str,
    hostname: &'a str,
    port: u16,
    remote_user: &'a str,
    local_user: &'a str,
    home: &'a str,
}

/// Expands `%%`, `%h`, `%n`, `%p`, `%r`, `%u` and `%d` in a single pass, so that
/// expanded values are never expanded again. Unknown tokens are kept verbatim.
fn expand_tokens(input: &str, tokens: &Tokens<'_>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // A trailing lone `%` is kept as is.
            Some('%') | None => out.push('%'),
            Some('h') => out.push_str(tokens.hostname),
            Some('n') => out.push_str(tokens.alias),
            Some('p') => {
                let _ = write!(out, "{}", tokens.port);
            }
            Some('r') => out.push_str(tokens.remote_user),
            Some('u') => out.push_str(tokens.local_user),
            Some('d') => out.push_str(tokens.home),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

fn expand_path(raw: &str, tokens: &Tokens<'_>, home: Option<&Path>) -> PathBuf {
    let expanded = expand_tokens(strip_quotes(raw), tokens);
    expand_home(&expanded, home)
}

fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if path == "~" => home.map_or_else(|| PathBuf::from(path), Path::to_path_buf),
        _ => PathBuf::from(path),
    }
}

fn strip_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
}

/// Splits a configuration line into keyword and argument. Both `Key Value` and
/// `Key=Value` (with optional whitespace around `=`) are accepted.
fn split_keyword(line: &str) -> (&str, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').map_or(rest, str::trim_start);
    (keyword, rest.trim_end())
}

/// Rewrites `ssh_config` text into the subset understood by russh-config.
fn preprocess(text: &str, base_dir: &Path, home: Option<&Path>, depth: u8, out: &mut String) {
    // The section header currently in effect, re-emitted after an `Include` so the
    // lines following it keep their original scope.
    let mut section: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, value) = split_keyword(line);
        if value.is_empty() {
            continue;
        }
        let lower = keyword.to_ascii_lowercase();
        match lower.as_str() {
            "host" => {
                let header = format!(
                    "Host {}",
                    value.split_whitespace().collect::<Vec<_>>().join(" ")
                );
                out.push_str(&header);
                out.push('\n');
                section = Some(header);
            }
            "match" => {
                // `Match all` applies everywhere (commonly used to `Include` at the
                // top level); other criteria are not supported and their block is
                // disabled with a pattern that only consists of a negation.
                let header = if value.eq_ignore_ascii_case("all") {
                    "Host *".to_owned()
                } else {
                    tracing::warn!(
                        criteria = value,
                        "ssh_config: `Match` is not supported; block ignored"
                    );
                    "Host !*".to_owned()
                };
                out.push_str(&header);
                out.push('\n');
                section = Some(header);
            }
            "include" => {
                if depth >= MAX_INCLUDE_DEPTH {
                    tracing::warn!("ssh_config: `Include` nested too deeply, ignoring");
                    continue;
                }
                for pattern in value.split_whitespace() {
                    for file in expand_include(strip_quotes(pattern), base_dir, home) {
                        match std::fs::read_to_string(&file) {
                            Ok(included) => preprocess(&included, base_dir, home, depth + 1, out),
                            Err(error) => {
                                tracing::warn!(file = %file.display(), %error, "ssh_config: cannot read include");
                            }
                        }
                    }
                }
                // Restore the scope that was active before the include.
                let header = section.clone().unwrap_or_else(|| "Host *".to_owned());
                out.push_str(&header);
                out.push('\n');
                section = Some(header);
            }
            _ => {
                if section.is_none() {
                    // Options before the first `Host` apply to every host.
                    let header = "Host *".to_owned();
                    out.push_str(&header);
                    out.push('\n');
                    section = Some(header);
                }
                out.push_str(keyword);
                out.push(' ');
                out.push_str(value);
                out.push('\n');
            }
        }
    }
}

/// Resolves an `Include` argument to files, sorted like glob(3) does.
/// Wildcards are supported in the last path component only.
fn expand_include(pattern: &str, base_dir: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let path = expand_home(pattern, home);
    let path = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    let Some(file_pattern) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !file_pattern.contains(['*', '?']) {
        return vec![path];
    }
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    if dir.to_string_lossy().contains(['*', '?']) {
        tracing::warn!(
            pattern,
            "ssh_config: wildcards in Include directories are not supported"
        );
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| crate::glob::matches(file_pattern, name, false))
        })
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_text(config: &str, options: &SftpOptions) -> ResolvedTarget {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config");
        std::fs::write(&path, config).expect("write config");
        let mut options = options.clone();
        options.ssh_config = SshConfigSource::File(path);
        resolve(&options).expect("resolve")
    }

    #[test]
    fn parses_destinations() {
        let o = SftpOptions::parse_destination("alice@example.org:2222").expect("parse");
        assert_eq!(
            (o.user.as_deref(), o.host.as_str(), o.port),
            (Some("alice"), "example.org", Some(2222))
        );
        let o = SftpOptions::parse_destination("example.org").expect("parse");
        assert_eq!(
            (o.user, o.host.as_str(), o.port),
            (None, "example.org", None)
        );
        let o = SftpOptions::parse_destination("[::1]:22").expect("parse");
        assert_eq!((o.host.as_str(), o.port), ("::1", Some(22)));
        let o = SftpOptions::parse_destination("::1").expect("parse");
        assert_eq!((o.host.as_str(), o.port), ("::1", None));
        for bad in ["", "@host", "host:0", "host:x", "host:70000", "a b", "[::1"] {
            assert!(SftpOptions::parse_destination(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn resolves_alias_with_normalized_syntax() {
        let config = "\
# global defaults come first and win
User=global
\tPort 2200

Host box
\tHostName=%h.example.org
\tIdentityFile ~/.ssh/box_%r
\tPort 2222

Match host box
\tUser ignored
";
        let target = resolve_text(config, &SftpOptions::new("box"));
        assert_eq!(target.hostname, "box.example.org");
        assert_eq!(target.user, "global");
        assert_eq!(target.port, 2200);
        assert!(target.identities_configured);
        let home = dirs::home_dir().expect("home");
        assert_eq!(target.identity_files, vec![home.join(".ssh/box_global")]);
    }

    #[test]
    fn explicit_options_win() {
        let config = "Host box\n  User configured\n  Port 2222\n";
        let mut options = SftpOptions::new("box");
        options.user = Some("explicit".into());
        options.port = Some(22);
        let target = resolve_text(config, &options);
        assert_eq!((target.user.as_str(), target.port), ("explicit", 22));
        assert_eq!(target.label(), "explicit@box");
    }

    #[test]
    fn inlines_includes_and_keeps_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("conf.d")).expect("mkdir");
        std::fs::write(
            dir.path().join("conf.d/10-a"),
            "Host a\n  HostName a.example\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("conf.d/20-b"),
            "Host b\n  HostName b.example\n",
        )
        .expect("write");
        let main = dir.path().join("config");
        std::fs::write(&main, "Host b\n  Include conf.d/*\n  User bob\n").expect("write");

        let mut options = SftpOptions::new("b");
        options.ssh_config = SshConfigSource::File(main);
        let target = resolve(&options).expect("resolve");
        assert_eq!(target.hostname, "b.example");
        assert_eq!(target.user, "bob");

        options.host = "a".into();
        let target = resolve(&options).expect("resolve");
        assert_eq!(target.hostname, "a.example");
        assert_ne!(target.user, "bob");
    }

    #[test]
    fn proxy_settings() {
        let target = resolve_text(
            "Host p\n  ProxyCommand nc -X 5 -x proxy:1080 %h %p\n",
            &SftpOptions::new("p"),
        );
        assert_eq!(
            target.proxy_command.as_deref(),
            Some("nc -X 5 -x proxy:1080 p 22")
        );

        let target = resolve_text("Host p\n  ProxyCommand none\n", &SftpOptions::new("p"));
        assert_eq!(target.proxy_command, None);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config");
        std::fs::write(&path, "Host j\n  ProxyJump bastion\n").expect("write");
        let mut options = SftpOptions::new("j");
        options.ssh_config = SshConfigSource::File(path);
        assert!(matches!(resolve(&options), Err(Error::Unsupported(_))));
    }

    #[test]
    fn match_all_applies_and_host_key_alias_is_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("extra"),
            "Host team/box\n  HostName 10.1.2.3\n  HostKeyAlias box-key\n",
        )
        .expect("write");
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "Host first\n  User a\nMatch all\n  Include extra\nMatch host other\n  User nope\n",
        )
        .expect("write");
        let mut options = SftpOptions::new("team/box");
        options.ssh_config = SshConfigSource::File(path);
        let target = resolve(&options).expect("resolve");
        assert_eq!(target.hostname, "10.1.2.3");
        assert_eq!(target.host_key_alias.as_deref(), Some("box-key"));
        assert_ne!(target.user, "nope");

        options.host = "first".into();
        let target = resolve(&options).expect("resolve");
        assert_eq!(target.host_key_alias, None);
    }

    #[test]
    fn lists_configured_hosts() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("extra"), "Host included\n  HostName x\n").expect("write");
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "User global\nHost web db\n  Port 1\nHost *.lan !bad\nInclude extra\nHost web\nMatch all\n",
        )
        .expect("write");
        let hosts = configured_hosts(&SshConfigSource::File(path)).expect("hosts");
        assert_eq!(hosts, ["web", "db", "included"]);
        assert_eq!(
            configured_hosts(&SshConfigSource::Disabled).expect("hosts"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn rejects_names_that_could_inject() {
        let config = "Host *\n  ProxyCommand nc %h %p\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config");
        std::fs::write(&path, config).expect("write");
        for (host, user) in [
            ("a;touch x", "u"),
            ("a$(id)", "u"),
            ("-oProxyCommand=x", "u"),
            ("ok", "u`id`"),
        ] {
            let mut options = SftpOptions::new(host);
            options.user = Some(user.into());
            options.ssh_config = SshConfigSource::File(path.clone());
            assert!(
                matches!(resolve(&options), Err(Error::Config(_))),
                "{host} / {user}"
            );
        }

        let mut options = SftpOptions::new("host\nevil ssh-ed25519 AAAA");
        options.ssh_config = SshConfigSource::Disabled;
        assert!(matches!(resolve(&options), Err(Error::Config(_))));

        let mut options = SftpOptions::new("ok-host.example");
        options.user = Some("first.last@corp".into());
        options.ssh_config = SshConfigSource::File(path);
        let target = resolve(&options).expect("safe names pass");
        assert_eq!(
            target.proxy_command.as_deref(),
            Some("nc ok-host.example 22")
        );
    }

    #[test]
    fn default_identities_and_known_hosts() {
        let target = resolve_text("", &SftpOptions::new("plain"));
        assert!(!target.identities_configured);
        assert!(
            target
                .identity_files
                .iter()
                .any(|p| p.ends_with("id_ed25519"))
        );
        assert!(
            target
                .user_known_hosts
                .iter()
                .any(|p| p.ends_with(".ssh/known_hosts"))
        );
        assert_ne!(target.global_known_hosts, Vec::<PathBuf>::new());

        let target = resolve_text(
            "Host k\n  UserKnownHostsFile ~/a_hosts ~/b_hosts\n",
            &SftpOptions::new("k"),
        );
        let home = dirs::home_dir().expect("home");
        assert_eq!(
            target.user_known_hosts,
            vec![home.join("a_hosts"), home.join("b_hosts")]
        );
    }

    #[test]
    fn token_expansion_is_single_pass() {
        let tokens = Tokens {
            alias: "%h",
            hostname: "host",
            port: 22,
            remote_user: "r",
            local_user: "u",
            home: "/home/u",
        };
        assert_eq!(
            expand_tokens("%n %%h %h:%p %r %u %d %x %", &tokens),
            "%h %h host:22 r u /home/u %x %"
        );
    }
}
