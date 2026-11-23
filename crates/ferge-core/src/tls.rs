//! TLS for the protocols that use it (FTPS, later S3 and WebDAV): the
//! operating system decides which certificates it trusts, and a certificate
//! it does not trust (self-signed, expired, for another name) is put to the
//! user with everything needed to judge it, like an unknown host key.
//!
//! An accepted certificate is pinned by its SHA-256 fingerprint: for the
//! rest of the session, and in [`KnownCertificates`] if the user asked to
//! remember it. A remembered certificate that changes is refused, unless the
//! system trusts the new one. Unencrypted connections are a choice made the
//! same way, per server.

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::error::{CertificateError, Error, Result};
use crate::interaction::{CertificatePrompt, HostKeyDecision, Interaction, PlaintextPrompt};

/// A server as TLS trust is recorded for it: `host:port`, the host in lower
/// case and IPv6 addresses in brackets.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerId {
    pub host: String,
    pub port: u16,
}

impl ServerId {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.to_ascii_lowercase(),
            port,
        }
    }

    fn key(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The name the certificate is checked against.
    #[cfg(test)]
    pub(crate) fn server_name(&self) -> Result<ServerName<'static>> {
        ServerName::try_from(self.host.clone())
            .map_err(|_| Error::Config(format!("invalid host name {:?}", self.host)))
    }
}

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

/// The SHA-256 fingerprint of a certificate (of its DER encoding).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(certificate: &[u8]) -> Self {
        Self(Sha256::digest(certificate).into())
    }

    /// Parses the form [`Display`](std::fmt::Display) writes.
    fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix("SHA256:")?;
        let mut bytes = [0; 32];
        let mut parts = hex.split(':');
        for byte in &mut bytes {
            *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
        }
        parts.next().is_none().then_some(Self(bytes))
    }
}

impl std::fmt::Display for Fingerprint {
    /// `SHA256:AB:CD:…`, as `openssl x509 -fingerprint -sha256` writes it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SHA256")?;
        for byte in self.0 {
            write!(f, ":{byte:02X}")?;
        }
        Ok(())
    }
}

/// What was decided for a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trust {
    /// This certificate, although the system does not trust it.
    Certificate(Fingerprint),
    /// Connecting without TLS.
    Plaintext,
}

/// The certificates (and unencrypted connections) the user chose to trust,
/// one line per server: `host:port SHA256:AB:…` or `host:port plaintext`.
#[derive(Clone, Debug)]
pub struct KnownCertificates {
    path: Option<PathBuf>,
}

impl KnownCertificates {
    /// `known_certificates` in ferge's data directory.
    pub fn default_path() -> Option<PathBuf> {
        Some(dirs::data_dir()?.join("ferge").join("known_certificates"))
    }

    /// The file at `path`, created when something is remembered.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    /// Remembers nothing: every untrusted certificate is asked about.
    pub fn none() -> Self {
        Self { path: None }
    }

    /// The default file, if there is a data directory.
    pub fn user() -> Self {
        Self {
            path: Self::default_path(),
        }
    }

    /// What is recorded for `server`, with the line it is on.
    async fn lookup(&self, server: &ServerId) -> Result<Option<(Trust, usize)>> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CertificateError::Unreadable {
                    path: path.clone(),
                    source,
                }
                .into());
            }
        };
        let key = server.key();
        for (index, line) in text.lines().enumerate() {
            let mut fields = line.split_whitespace();
            if fields.next() != Some(key.as_str()) {
                continue;
            }
            let trust = match fields.next() {
                Some("plaintext") => Trust::Plaintext,
                Some(fingerprint) => match Fingerprint::parse(fingerprint) {
                    Some(fingerprint) => Trust::Certificate(fingerprint),
                    None => continue,
                },
                None => continue,
            };
            return Ok(Some((trust, index + 1)));
        }
        Ok(None)
    }

    async fn remember(&self, server: &ServerId, trust: Trust) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let line = match trust {
            Trust::Certificate(fingerprint) => format!("{} {fingerprint}\n", server.key()),
            Trust::Plaintext => format!("{} plaintext\n", server.key()),
        };
        let save = |source| CertificateError::Save {
            path: path.clone(),
            source,
        };
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(save)?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(save)?;
        file.write_all(line.as_bytes()).await.map_err(save)?;
        file.flush().await.map_err(save)?;
        Ok(())
    }

    fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or(Path::new("(nowhere)"))
    }
}

/// TLS settings for one server: what the system trusts, plus the
/// certificate the user accepted for it. Every connection to the server in
/// a session uses the same one, so data connections can resume the control
/// connection's TLS session, as FTP servers often require.
#[derive(Clone, Debug)]
pub(crate) struct TlsClient {
    /// TLS 1.3 and 1.2.
    config: Arc<ClientConfig>,
    /// TLS 1.2 only, see [`Versions::Resumable`].
    resumable: Arc<ClientConfig>,
    verifier: Arc<Verifier>,
}

/// The TLS versions a connection offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Versions {
    /// TLS 1.3 and 1.2; the server picks, usually 1.3.
    Any,
    /// Only TLS 1.2, whose sessions can be resumed any number of times.
    /// FTP servers want every data connection to resume the control
    /// connection's session, but rustls resumes a TLS 1.3 session only
    /// once per ticket: with a server that hands out no new tickets on data
    /// connections (ProFTPD), the third data connection would be refused.
    Resumable,
}

impl TlsClient {
    fn new(system: Arc<dyn ServerCertVerifier>, accepted: Option<Fingerprint>) -> Result<Self> {
        let verifier = Arc::new(Verifier {
            system,
            accepted,
            rejected: Mutex::new(None),
        });
        let config = |versions: &[&'static rustls::SupportedProtocolVersion]| {
            ClientConfig::builder_with_provider(provider())
                .with_protocol_versions(versions)
                .map_err(|error| Error::protocol("setting up TLS", error))
                .map(|builder| {
                    Arc::new(
                        builder
                            .dangerous()
                            .with_custom_certificate_verifier(
                                Arc::clone(&verifier) as Arc<dyn ServerCertVerifier>
                            )
                            .with_no_client_auth(),
                    )
                })
        };
        Ok(Self {
            config: config(rustls::DEFAULT_VERSIONS)?,
            resumable: config(&[&rustls::version::TLS12])?,
            verifier,
        })
    }

    #[cfg(test)]
    pub(crate) fn connector(&self) -> tokio_rustls::TlsConnector {
        tokio_rustls::TlsConnector::from(Arc::clone(&self.config))
    }

    /// A connector with the same trust but its own TLS session cache, for
    /// a connection whose data connections must resume exactly its session
    /// (FTP servers check that), not one of a parallel connection.
    pub(crate) fn connector_of_its_own(&self, versions: Versions) -> tokio_rustls::TlsConnector {
        let mut config = ClientConfig::clone(match versions {
            Versions::Any => &self.config,
            Versions::Resumable => &self.resumable,
        });
        config.resumption = rustls::client::Resumption::in_memory_sessions(16);
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }
}

/// Whether a handshake failed because the server does not speak the
/// versions offered, as a server that knows only TLS 1.3 answers an offer of
/// [`Versions::Resumable`]. Goes by the text of `error` and its sources,
/// since some libraries pass on no more than that.
pub(crate) fn is_version_refusal(error: &(dyn std::error::Error + 'static)) -> bool {
    let refusals = [
        rustls::Error::AlertReceived(rustls::AlertDescription::ProtocolVersion),
        rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::ServerTlsVersionIsDisabledByOurConfig,
        ),
    ]
    .map(|refusal| refusal.to_string());
    std::iter::successors(Some(error), |error| error.source()).any(|error| {
        let text = error.to_string();
        refusals
            .iter()
            .any(|refusal| text.contains(refusal.as_str()))
    })
}

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    Arc::clone(PROVIDER.get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider())))
}

/// Where the system's trust comes from.
#[derive(Clone, Debug, Default)]
pub enum SystemTrust {
    /// The operating system's verifier and trust store.
    #[default]
    Platform,
    /// The operating system's, plus these root certificates (DER), for tests.
    WithRoots(Vec<Vec<u8>>),
}

impl SystemTrust {
    /// The system verifier. The platform's is made once: on Linux, making
    /// it reads every root certificate from disk.
    async fn verifier(&self) -> Result<Arc<dyn ServerCertVerifier>> {
        static PLATFORM: tokio::sync::OnceCell<Arc<dyn ServerCertVerifier>> =
            tokio::sync::OnceCell::const_new();
        let make = |roots: Vec<Vec<u8>>| async move {
            tokio::task::spawn_blocking(move || {
                let roots = roots.into_iter().map(CertificateDer::from);
                rustls_platform_verifier::Verifier::new_with_extra_roots(roots, provider())
                    .map(|verifier| Arc::new(verifier) as Arc<dyn ServerCertVerifier>)
                    .map_err(|error| Error::protocol("loading the system's certificates", error))
            })
            .await
            .map_err(|error| Error::protocol("loading the system's certificates", error))?
        };
        match self {
            Self::Platform => PLATFORM
                .get_or_try_init(|| make(Vec::new()))
                .await
                .map(Arc::clone),
            Self::WithRoots(roots) => make(roots.clone()).await,
        }
    }
}

/// Where TLS trust is decided and recorded.
#[derive(Clone, Debug, Default)]
pub struct TlsOptions {
    pub system: SystemTrust,
    pub known: Option<KnownCertificates>,
}

impl TlsOptions {
    fn known(&self) -> KnownCertificates {
        self.known.clone().unwrap_or_else(KnownCertificates::user)
    }
}

/// Runs `attempt` (a connection with a TLS handshake) with the TLS settings
/// for `server`. If the server presents a certificate the system does not
/// trust, the user is asked; if they accept it, `attempt` runs again with
/// exactly that certificate pinned. Returns what `attempt` returned and the
/// settings to use for further connections to the server.
pub(crate) async fn establish<T, F, Fut>(
    server: &ServerId,
    options: &TlsOptions,
    interaction: &dyn Interaction,
    mut attempt: F,
) -> Result<(T, TlsClient)>
where
    F: FnMut(TlsClient) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let known = options.known();
    let system = options.system.verifier().await?;
    let remembered = match known.lookup(server).await? {
        Some((Trust::Certificate(fingerprint), line)) => Some((fingerprint, line)),
        _ => None,
    };
    let client = TlsClient::new(Arc::clone(&system), remembered.map(|(print, _)| print))?;
    let error = match attempt(client.clone()).await {
        Ok(value) => return Ok((value, client)),
        Err(error) => error,
    };
    let Some(rejected) = client.verifier.take_rejected() else {
        return Err(error);
    };
    let fingerprint = Fingerprint::of(&rejected.certificate);
    tracing::info!(
        %server,
        %fingerprint,
        reason = %rejected.error,
        remembered = remembered.is_some(),
        "the system does not trust the certificate"
    );
    if let Some((_, line)) = remembered {
        return Err(CertificateError::Changed {
            server: server.key(),
            fingerprint: fingerprint.to_string(),
            file: known.path().to_owned(),
            line,
        }
        .into());
    }
    let problem = describe(&rejected.error, &rejected.certificate, SystemTime::now());
    if !problem.may_be_accepted {
        return Err(CertificateError::Untrusted {
            server: server.key(),
            problem: problem.text,
        }
        .into());
    }
    let prompt = prompt(
        server,
        &rejected.certificate,
        fingerprint,
        problem.text.clone(),
    );
    let decision = interaction.confirm_certificate(prompt).await;
    if decision == HostKeyDecision::Reject {
        return Err(CertificateError::Rejected {
            server: server.key(),
            fingerprint: fingerprint.to_string(),
            problem: problem.text,
        }
        .into());
    }
    let client = TlsClient::new(system, Some(fingerprint))?;
    let value = match attempt(client.clone()).await {
        Ok(value) => value,
        Err(error) => {
            // A different certificate than the one just accepted: the
            // server changed it in between, or someone is in the way.
            return Err(match client.verifier.take_rejected() {
                Some(again) => CertificateError::Changed {
                    server: server.key(),
                    fingerprint: Fingerprint::of(&again.certificate).to_string(),
                    file: known.path().to_owned(),
                    line: 0,
                }
                .into(),
                None => error,
            });
        }
    };
    if decision == HostKeyDecision::AcceptAndRemember {
        known
            .remember(server, Trust::Certificate(fingerprint))
            .await?;
    }
    Ok((value, client))
}

/// Asks whether connecting to `server` without encryption is fine, unless
/// that was decided before. `reason` says why there is no TLS.
pub(crate) async fn allow_plaintext(
    server: &ServerId,
    protocol: &'static str,
    reason: &str,
    options: &TlsOptions,
    interaction: &dyn Interaction,
) -> Result<()> {
    let known = options.known();
    if let Some((Trust::Plaintext, _)) = known.lookup(server).await? {
        return Ok(());
    }
    let prompt = PlaintextPrompt {
        server: server.key(),
        protocol,
        reason: reason.to_owned(),
    };
    match interaction.confirm_plaintext(prompt).await {
        HostKeyDecision::Reject => Err(CertificateError::PlaintextRefused {
            server: server.key(),
        }
        .into()),
        HostKeyDecision::AcceptOnce => Ok(()),
        HostKeyDecision::AcceptAndRemember => known.remember(server, Trust::Plaintext).await,
    }
}

/// A certificate the system did not trust, and why.
#[derive(Debug)]
struct Rejected {
    certificate: Vec<u8>,
    error: rustls::Error,
}

/// The system's verifier, which also accepts the one certificate the user
/// accepted, and keeps what it rejected for the question.
#[derive(Debug)]
struct Verifier {
    system: Arc<dyn ServerCertVerifier>,
    accepted: Option<Fingerprint>,
    rejected: Mutex<Option<Rejected>>,
}

impl Verifier {
    fn take_rejected(&self) -> Option<Rejected> {
        self.rejected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.accepted == Some(Fingerprint::of(end_entity)) {
            return Ok(ServerCertVerified::assertion());
        }
        self.system
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .inspect_err(|error| {
                *self.rejected.lock().unwrap_or_else(PoisonError::into_inner) = Some(Rejected {
                    certificate: end_entity.to_vec(),
                    error: error.clone(),
                });
            })
    }

    // Signatures are always checked, also for an accepted certificate: it
    // proves the server holds the certificate's key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.system.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.system.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.system.supported_verify_schemes()
    }
}

/// Why the system did not trust a certificate, in words that complete
/// "the certificate …".
struct Problem {
    text: String,
    /// Whether the user may decide to trust it anyway. A revoked
    /// certificate or a broken signature is never offered.
    may_be_accepted: bool,
}

/// Why the system rejected `certificate` with `error`. Where the system
/// does not say clearly (macOS reports a self-signed certificate only by an
/// error number), the certificate itself tells.
fn describe(error: &rustls::Error, certificate: &[u8], now: SystemTime) -> Problem {
    use rustls::CertificateError as E;
    let evident = || evident_problem(certificate, now);
    let unclear = |what: String| (evident().map_or(what, str::to_owned), true);
    let (text, may_be_accepted) = match error {
        rustls::Error::InvalidCertificate(error) => match error {
            E::UnknownIssuer => (
                evident().map_or_else(
                    || {
                        "is not signed by an authority this computer trusts (it may be \
                         self-signed)"
                            .into()
                    },
                    str::to_owned,
                ),
                true,
            ),
            E::Expired | E::ExpiredContext { .. } => ("has expired".into(), true),
            E::NotValidYet | E::NotValidYetContext { .. } => ("is not valid yet".into(), true),
            E::NotValidForName | E::NotValidForNameContext { .. } => {
                ("was issued for a different name".into(), true)
            }
            E::InvalidPurpose | E::InvalidPurposeContext { .. } => {
                ("is not meant for servers".into(), true)
            }
            E::Revoked => ("has been revoked".into(), false),
            E::BadSignature => ("has a broken signature".into(), false),
            // What `openssl req -x509` makes: a self-signed certificate
            // that says it is an authority.
            E::Other(other)
                if matches!(
                    other.0.downcast_ref::<webpki::Error>(),
                    Some(webpki::Error::CaUsedAsEndEntity)
                ) =>
            {
                ("is self-signed".into(), true)
            }
            other => unclear(format!("could not be verified ({other:?})")),
        },
        other => unclear(format!("could not be verified ({other})")),
    };
    Problem {
        text,
        may_be_accepted,
    }
}

/// What makes `certificate` untrustworthy on its face at `now`: that it
/// has expired, is not valid yet, or names itself as its issuer.
fn evident_problem(certificate: &[u8], now: SystemTime) -> Option<&'static str> {
    let (_, parsed) = x509_parser::parse_x509_certificate(certificate).ok()?;
    let now = i64::try_from(now.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    let validity = parsed.validity();
    if now > validity.not_after.timestamp() {
        Some("has expired")
    } else if now < validity.not_before.timestamp() {
        Some("is not valid yet")
    } else if is_self_issued(&parsed) {
        Some("is self-signed")
    } else {
        None
    }
}

/// Whether `certificate` names itself as its issuer and is signed with its
/// own key.
fn is_self_issued(certificate: &x509_parser::certificate::X509Certificate<'_>) -> bool {
    certificate.subject().as_raw() == certificate.issuer().as_raw()
        && certificate.verify_signature(None).is_ok()
}

/// The question about `certificate`, with what it says about itself.
fn prompt(
    server: &ServerId,
    certificate: &[u8],
    fingerprint: Fingerprint,
    problem: String,
) -> CertificatePrompt {
    let mut prompt = CertificatePrompt {
        server: server.key(),
        problem,
        subject: String::new(),
        issuer: String::new(),
        names: Vec::new(),
        valid_from: None,
        valid_until: None,
        fingerprint: fingerprint.to_string(),
    };
    let Ok((_, parsed)) = x509_parser::parse_x509_certificate(certificate) else {
        prompt.subject = "(unreadable)".into();
        return prompt;
    };
    prompt.subject = parsed.subject().to_string();
    prompt.issuer = parsed.issuer().to_string();
    let time = |seconds: i64| {
        u64::try_from(seconds)
            .ok()
            .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
    };
    prompt.valid_from = time(parsed.validity().not_before.timestamp());
    prompt.valid_until = time(parsed.validity().not_after.timestamp());
    if let Ok(Some(names)) = parsed.subject_alternative_name() {
        for name in &names.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::DNSName(name) => {
                    prompt.names.push((*name).to_owned());
                }
                x509_parser::extensions::GeneralName::IPAddress(bytes) => {
                    if let Some(address) = ip_address(bytes) {
                        prompt.names.push(address);
                    }
                }
                _ => {}
            }
        }
    }
    prompt
}

fn ip_address(bytes: &[u8]) -> Option<String> {
    match bytes.len() {
        4 => {
            let octets: [u8; 4] = bytes.try_into().ok()?;
            Some(std::net::Ipv4Addr::from(octets).to_string())
        }
        16 => {
            let octets: [u8; 16] = bytes.try_into().ok()?;
            Some(std::net::Ipv6Addr::from(octets).to_string())
        }
        _ => {
            let mut text = String::new();
            for byte in bytes {
                let _ = write!(text, "{byte:02x}");
            }
            Some(text)
        }
    }
}

#[cfg(test)]
mod tests;
