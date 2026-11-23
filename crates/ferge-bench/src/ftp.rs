//! FTP against the two test servers: vsftpd, which lists with `LIST` and
//! sorts the folder before it sends the first line, and ProFTPD, which lists
//! with `MLSD` as it reads the folder. Reported, not judged: how soon the
//! first rows come is up to the server.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use ferge_core::destination::FtpTls;
use ferge_core::interaction::{
    Account, CertificatePrompt, HostKeyDecision, HostKeyPrompt, Interaction, KeyboardPrompt,
};
use ferge_core::remote::ftp::{FtpOptions, FtpRemote};
use ferge_core::tls::{Fingerprint, KnownCertificates, SystemTrust, TlsOptions};
use ferge_core::{FtpTarget, Remote, RemotePath};
use secrecy::SecretString;

/// One of the test servers.
pub(crate) struct Server {
    pub(crate) name: &'static str,
    pub(crate) port: u16,
}

/// The servers, at the ports `testserver/up.sh` uses unless told otherwise.
pub(crate) fn servers() -> [Server; 2] {
    let port = |variable: &str, default: u16| {
        std::env::var(variable)
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(default)
    };
    [
        Server {
            name: "vsftpd, LIST",
            port: port("FERGE_TEST_FTP_PORT", 2121),
        },
        Server {
            name: "ProFTPD, MLSD",
            port: port("FERGE_TEST_FTP_MLSD_PORT", 2124),
        },
    ]
}

/// Accepts the test server's certificate, and nothing else.
#[derive(Debug)]
struct TestCertificate(String);

#[async_trait]
impl Interaction for TestCertificate {
    async fn confirm_host_key(&self, _prompt: HostKeyPrompt) -> HostKeyDecision {
        HostKeyDecision::Reject
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
        if prompt.fingerprint == self.0 {
            HostKeyDecision::AcceptOnce
        } else {
            HostKeyDecision::Reject
        }
    }
}

/// The fingerprint of the certificate `up.sh` saved.
fn test_certificate(state: &Path) -> anyhow::Result<String> {
    let file = state.join("ftp-cert.pem");
    let pem = std::fs::read_to_string(&file)
        .with_context(|| format!("{} (run ./testserver/up.sh)", file.display()))?;
    let base64: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = <base64ct::Base64 as base64ct::Encoding>::decode_vec(&base64)
        .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;
    Ok(Fingerprint::of(&der).to_string())
}

/// Logs in as `ferge` over explicit TLS; `None` if the server does not
/// answer.
pub(crate) async fn connect(
    server: &Server,
    state: &Path,
) -> anyhow::Result<Option<Arc<dyn Remote>>> {
    if tokio::net::TcpStream::connect(("127.0.0.1", server.port))
        .await
        .is_err()
    {
        return Ok(None);
    }
    let mut target = FtpTarget::new("127.0.0.1", FtpTls::Explicit)?;
    target.port = Some(server.port);
    target.user = Some("ferge".into());
    let mut options = FtpOptions::new(&target);
    options.password = Some(SecretString::from("ferge"));
    options.tls_options = TlsOptions {
        system: SystemTrust::Platform,
        known: Some(KnownCertificates::none()),
    };
    let interaction = Arc::new(TestCertificate(test_certificate(state)?));
    let remote = FtpRemote::connect(options, interaction)
        .await
        .with_context(|| format!("logging in to {} on port {}", server.name, server.port))?;
    Ok(Some(Arc::new(remote)))
}

/// Downloads `path` with `curl`, which resumes the TLS session on data
/// connections as the servers require; `None` if it is not installed or
/// fails.
pub(crate) async fn curl_get(
    server: &Server,
    path: &RemotePath,
    size: u64,
    scratch: &Path,
) -> Option<Duration> {
    let target = scratch.join("curl.bin");
    let started = Instant::now();
    let status = tokio::process::Command::new("curl")
        .args([
            "--silent",
            "--ssl-reqd",
            "--insecure",
            "--user",
            "ferge:ferge",
        ])
        .arg("--output")
        .arg(&target)
        // Two slashes: the path is absolute, not below the login folder.
        .arg(format!("ftp://127.0.0.1:{}/{path}", server.port))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .ok()?;
    let elapsed = started.elapsed();
    let length = std::fs::metadata(&target).ok()?.len();
    let _ = std::fs::remove_file(&target);
    (status.success() && length == size).then_some(elapsed)
}
