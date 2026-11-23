//! Certificate decisions against a local TLS server.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use secrecy::SecretString;
use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::interaction::{Account, HostKeyPrompt, KeyboardPrompt};

/// A certificate and its key, as a server presents them.
struct Identity {
    der: Vec<u8>,
    key: Vec<u8>,
}

fn self_signed() -> Identity {
    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let certificate = params.self_signed(&key).unwrap();
    Identity {
        der: certificate.der().to_vec(),
        key: key.serialize_der(),
    }
}

/// A certificate authority and a server certificate it signed.
fn signed() -> (Vec<u8>, Identity) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let certificate = params.signed_by(&key, &issuer).unwrap();
    (
        ca.der().to_vec(),
        Identity {
            der: certificate.der().to_vec(),
            key: key.serialize_der(),
        },
    )
}

/// Serves `identity` on a local port: every connection gets "hello".
async fn serve(identity: &Identity) -> u16 {
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(identity.der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key.clone())),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut stream) = acceptor.accept(stream).await {
                    let _ = stream.write_all(b"hello").await;
                    let _ = stream.shutdown().await;
                }
            });
        }
    });
    port
}

/// Connects and reads what the server says.
async fn hello(client: TlsClient, port: u16) -> Result<String> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|error| Error::protocol("connecting", error))?;
    let name = ServerId::new("127.0.0.1", port).server_name()?;
    let mut stream = client
        .connector()
        .connect(name, stream)
        .await
        .map_err(|error| Error::protocol("TLS handshake", error))?;
    let mut text = String::new();
    stream
        .read_to_string(&mut text)
        .await
        .map_err(|error| Error::protocol("reading", error))?;
    Ok(text)
}

/// Gives one answer to every question and keeps the questions.
#[derive(Debug)]
struct Answer {
    decision: HostKeyDecision,
    asked: AtomicUsize,
    prompts: Mutex<Vec<CertificatePrompt>>,
}

impl Answer {
    fn new(decision: HostKeyDecision) -> Self {
        Self {
            decision,
            asked: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Interaction for Answer {
    async fn confirm_host_key(&self, _prompt: HostKeyPrompt) -> HostKeyDecision {
        unreachable!()
    }
    async fn key_passphrase(&self, _key_file: &Path, _retry: bool) -> Option<SecretString> {
        None
    }
    async fn password(&self, _account: &Account, _retry: bool) -> Option<SecretString> {
        None
    }
    async fn keyboard_interactive(
        &self,
        _account: &Account,
        _name: &str,
        _instructions: &str,
        _prompts: &[KeyboardPrompt],
    ) -> Option<Vec<SecretString>> {
        None
    }
    async fn confirm_certificate(&self, prompt: CertificatePrompt) -> HostKeyDecision {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(prompt);
        self.decision
    }
    async fn confirm_plaintext(&self, _prompt: PlaintextPrompt) -> HostKeyDecision {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.decision
    }
}

fn options(known: KnownCertificates) -> TlsOptions {
    TlsOptions {
        system: SystemTrust::Platform,
        known: Some(known),
    }
}

#[tokio::test]
async fn asks_about_an_untrusted_certificate_and_pins_exactly_that_one() {
    let identity = self_signed();
    let port = serve(&identity).await;
    let server = ServerId::new("127.0.0.1", port);
    let answer = Answer::new(HostKeyDecision::AcceptOnce);
    let dir = tempfile::tempdir().unwrap();
    let known = KnownCertificates::at(dir.path().join("known_certificates"));

    let (text, client) = establish(&server, &options(known), &answer, |client| {
        hello(client, port)
    })
    .await
    .unwrap();
    assert_eq!(text, "hello");
    assert_eq!(answer.asked(), 1);
    let prompt = answer.prompts.lock().unwrap().pop().unwrap();
    assert_eq!(prompt.server, format!("127.0.0.1:{port}"));
    assert_eq!(
        prompt.fingerprint,
        Fingerprint::of(&identity.der).to_string()
    );
    assert!(prompt.problem.contains("not signed"), "{}", prompt.problem);
    assert_eq!(prompt.names, ["localhost", "127.0.0.1"]);
    assert!(prompt.valid_from.unwrap() < prompt.valid_until.unwrap());
    assert!(
        !dir.path().join("known_certificates").exists(),
        "accepted once only"
    );

    // Further connections of the session use the pinned certificate.
    assert_eq!(hello(client, port).await.unwrap(), "hello");
    // Another certificate on the same port is not covered by it.
    let other = serve(&self_signed()).await;
    let (_, pinned) = establish(&server, &options(KnownCertificates::none()), &answer, |c| {
        hello(c, port)
    })
    .await
    .unwrap();
    assert!(hello(pinned, other).await.is_err());
}

#[tokio::test]
async fn remembers_a_certificate_when_asked() {
    let identity = self_signed();
    let port = serve(&identity).await;
    let server = ServerId::new("127.0.0.1", port);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ferge/known_certificates");

    let remember = Answer::new(HostKeyDecision::AcceptAndRemember);
    establish(
        &server,
        &options(KnownCertificates::at(&file)),
        &remember,
        |client| hello(client, port),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("127.0.0.1:{port} {}\n", Fingerprint::of(&identity.der))
    );

    let reject = Answer::new(HostKeyDecision::Reject);
    let (text, _) = establish(
        &server,
        &options(KnownCertificates::at(&file)),
        &reject,
        |client| hello(client, port),
    )
    .await
    .unwrap();
    assert_eq!(text, "hello");
    assert_eq!(reject.asked(), 0, "remembered");
}

#[tokio::test]
async fn refuses_a_changed_certificate_without_asking() {
    let port = serve(&self_signed()).await;
    let server = ServerId::new("127.0.0.1", port);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("known_certificates");
    let earlier = Fingerprint::of(&self_signed().der);
    std::fs::write(
        &file,
        format!("other:21 {earlier}\n127.0.0.1:{port} {earlier}\n"),
    )
    .unwrap();

    let answer = Answer::new(HostKeyDecision::AcceptAndRemember);
    let error = establish(
        &server,
        &options(KnownCertificates::at(&file)),
        &answer,
        |client| hello(client, port),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            Error::Certificate(CertificateError::Changed { line: 2, .. })
        ),
        "{error:?}"
    );
    assert_eq!(answer.asked(), 0);
}

#[tokio::test]
async fn a_declined_certificate_fails_the_connection() {
    let port = serve(&self_signed()).await;
    let server = ServerId::new("127.0.0.1", port);
    let answer = Answer::new(HostKeyDecision::Reject);
    let error = establish(
        &server,
        &options(KnownCertificates::none()),
        &answer,
        |client| hello(client, port),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, Error::Certificate(CertificateError::Rejected { .. })),
        "{error:?}"
    );
    assert_eq!(answer.asked(), 1);
}

#[tokio::test]
async fn trusts_what_the_system_trusts_even_after_another_one_was_remembered() {
    let (ca, identity) = signed();
    let port = serve(&identity).await;
    let server = ServerId::new("127.0.0.1", port);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("known_certificates");
    // A self-signed certificate was accepted before; now the server has
    // one the system trusts.
    std::fs::write(
        &file,
        format!("127.0.0.1:{port} {}\n", Fingerprint::of(&self_signed().der)),
    )
    .unwrap();
    let answer = Answer::new(HostKeyDecision::Reject);
    let options = TlsOptions {
        system: SystemTrust::WithRoots(vec![ca]),
        known: Some(KnownCertificates::at(&file)),
    };
    let (text, _) = establish(&server, &options, &answer, |client| hello(client, port))
        .await
        .unwrap();
    assert_eq!(text, "hello");
    assert_eq!(answer.asked(), 0);
}

#[tokio::test]
async fn asks_once_before_connecting_without_encryption() {
    let server = ServerId::new("Example.org", 21);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("known_certificates");
    let known = || options(KnownCertificates::at(&file));

    let reject = Answer::new(HostKeyDecision::Reject);
    let error = allow_plaintext(&server, "FTP", "no AUTH TLS", &known(), &reject)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            Error::Certificate(CertificateError::PlaintextRefused { .. })
        ),
        "{error:?}"
    );

    let remember = Answer::new(HostKeyDecision::AcceptAndRemember);
    allow_plaintext(&server, "FTP", "no AUTH TLS", &known(), &remember)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "example.org:21 plaintext\n"
    );
    allow_plaintext(&server, "FTP", "no AUTH TLS", &known(), &reject)
        .await
        .unwrap();
    assert_eq!(reject.asked(), 1, "remembered");
}

#[test]
fn fingerprints_read_back() {
    let fingerprint = Fingerprint::of(b"certificate");
    let text = fingerprint.to_string();
    assert!(
        text.starts_with("SHA256:") && text.len() == 6 + 32 * 3,
        "{text}"
    );
    assert_eq!(Fingerprint::parse(&text), Some(fingerprint));
    assert_eq!(Fingerprint::parse("SHA256:AB"), None);
    assert_eq!(Fingerprint::parse(&format!("{text}:00")), None);
    assert_eq!(
        ServerId::new("::1", 990).to_string(),
        "[::1]:990",
        "IPv6 in brackets"
    );
}

#[tokio::test]
async fn calls_a_self_signed_authority_what_it_is() {
    // What `openssl req -x509` makes: self-signed, and marked as an
    // authority.
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let identity = Identity {
        der: params.self_signed(&key).unwrap().der().to_vec(),
        key: key.serialize_der(),
    };
    let port = serve(&identity).await;
    let answer = Answer::new(HostKeyDecision::AcceptOnce);
    establish(
        &ServerId::new("127.0.0.1", port),
        &options(KnownCertificates::none()),
        &answer,
        |client| hello(client, port),
    )
    .await
    .unwrap();
    let prompt = answer.prompts.lock().unwrap().pop().unwrap();
    assert_eq!(prompt.problem, "is self-signed");
}
