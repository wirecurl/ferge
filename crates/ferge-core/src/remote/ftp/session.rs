//! FTP control connections: opening and logging in, the pool they live in,
//! and the commands ferge sends on them.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use futures::future::BoxFuture;
use secrecy::{ExposeSecret as _, SecretString};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream, AsyncRustlsStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpResult, Mode, Status};
use tokio::net::TcpStream;
use tokio::sync::Notify;

use super::listing::{self, Format};
use super::{Features, FtpOptions, is_path_refusal, parse_size};
use crate::destination::FtpTls;
use crate::error::{Error, Result};
use crate::interaction::Interaction;
use crate::path::RemotePath;
use crate::tls::{self, ServerId, TlsClient, Versions};

type Stream = AsyncRustlsFtpStream;

/// The data connection of a transfer or listing.
pub(super) type Transfer = suppaftp::tokio::TransferStream<AsyncRustlsStream>;

/// How the connections to a server are secured, as the first one settled.
#[derive(Clone, Debug)]
enum Security {
    /// TLS with the versions the first connection settled on.
    Tls(TlsClient, Versions),
    /// The user agreed to go without encryption.
    Plain,
}

impl Security {
    fn versions(&self) -> Option<Versions> {
        match self {
            Self::Tls(_, versions) => Some(*versions),
            Self::Plain => None,
        }
    }
}

/// An idle connection is checked with `NOOP` before it is used again
/// after this long: servers drop idle connections.
const IDLE_CHECK: Duration = Duration::from_secs(30);

/// Logged-in connections to one server.
pub(super) struct Pool {
    pub(super) options: FtpOptions,
    pub(super) label: String,
    pub(super) home: RemotePath,
    pub(super) features: Features,
    user: String,
    password: SecretString,
    security: Security,
    /// Whether `LIST -a` works (it lists hidden files on most servers).
    list_hidden: AtomicBool,
    state: Mutex<State>,
    freed: Notify,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("label", &self.label)
            .field("features", &self.features)
            .finish_non_exhaustive()
    }
}

struct State {
    idle: Vec<Session>,
    /// Connections that exist, idle or leased.
    open: usize,
    /// How many may exist.
    limit: usize,
}

struct Session {
    ftp: Stream,
    used: Instant,
}

impl Pool {
    /// Opens the first connection: settles the security (asking about
    /// certificates and unencrypted connections) and the password.
    pub(super) async fn connect(
        options: FtpOptions,
        interaction: Arc<dyn Interaction>,
    ) -> Result<Self> {
        let interaction = interaction.as_ref();
        let (ftp, security) = secure(&options, interaction).await?;
        let account = options.account();
        let user = account.user.clone();
        let (mut ftp, password) = log_in(ftp, &options, &security, interaction).await?;
        let features = features(&mut ftp, &options).await;
        let ftp = prepare(ftp, &options, features).await?;
        let mut session = Session {
            ftp,
            used: Instant::now(),
        };
        let home = bounded(&options, "PWD", session.ftp.pwd())
            .await
            .map_err(|error| ftp_error(error, None, "PWD").0)?;
        let home = RemotePath::parse(&home).unwrap_or_else(|_| RemotePath::root());
        tracing::debug!(?features, %home, tls = ?security.versions(), "FTP server");
        session.used = Instant::now();
        let limit = options.max_connections.max(1);
        Ok(Self {
            label: account.to_string(),
            home,
            features,
            user,
            password,
            security,
            list_hidden: AtomicBool::new(true),
            state: Mutex::new(State {
                idle: vec![session],
                open: 1,
                limit,
            }),
            freed: Notify::new(),
            options,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A connection for one operation: an idle one, a new one while the
    /// limit allows, or the next one given back.
    pub(super) async fn session(pool: &Arc<Self>) -> Result<Lease> {
        enum Next {
            Reuse(Session),
            Open,
            Wait,
        }
        loop {
            let next = {
                let mut state = pool.state();
                if let Some(session) = state.idle.pop() {
                    Next::Reuse(session)
                } else if state.open < state.limit {
                    state.open += 1;
                    Next::Open
                } else {
                    Next::Wait
                }
            };
            match next {
                Next::Reuse(mut session) => {
                    if session.used.elapsed() > IDLE_CHECK
                        && bounded(&pool.options, "NOOP", session.ftp.noop())
                            .await
                            .is_err()
                    {
                        // Gone stale; open another one instead.
                        drop(session);
                        let mut state = pool.state();
                        state.open -= 1;
                        continue;
                    }
                    return Ok(Lease::new(pool, session));
                }
                Next::Open => match pool.open().await {
                    Ok(session) => return Ok(Lease::new(pool, session)),
                    Err(error) => {
                        let mut state = pool.state();
                        state.open -= 1;
                        if state.open == 0 {
                            return Err(error);
                        }
                        // The others work: the server allows no more.
                        tracing::debug!(%error, open = state.open, "FTP server allows no more connections");
                        state.limit = state.open;
                    }
                },
                Next::Wait => pool.freed.notified().await,
            }
        }
    }

    /// Opens and logs in one more connection like the first.
    async fn open(&self) -> Result<Session> {
        let mut ftp = open_with(&self.options, &self.security).await?;
        login(&mut ftp, &self.options, &self.user, &self.password).await?;
        let ftp = prepare(ftp, &self.options, self.features).await?;
        Ok(Session {
            ftp,
            used: Instant::now(),
        })
    }

    fn give_back(&self, session: Option<Session>) {
        let mut state = self.state();
        match session {
            Some(mut session) => {
                session.used = Instant::now();
                state.idle.push(session);
            }
            None => state.open -= 1,
        }
        drop(state);
        self.freed.notify_one();
    }
}

/// A connection taken from the pool; given back when dropped, unless it
/// broke.
pub(super) struct Lease {
    session: Option<Session>,
    pool: Arc<Pool>,
    broken: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let session = self.session.take().filter(|_| !self.broken);
        self.pool.give_back(session);
    }
}

impl Lease {
    fn new(pool: &Arc<Pool>, session: Session) -> Self {
        Self {
            session: Some(session),
            pool: Arc::clone(pool),
            broken: false,
        }
    }

    /// Closes the connection instead of giving it back: it is in the
    /// middle of something.
    pub(super) fn discard(mut self) {
        self.broken = true;
    }

    pub(super) fn timeout(&self) -> Duration {
        self.pool.options.command_timeout
    }

    /// Runs one command, bounded in time. A connection that failed (rather
    /// than answering no) is not reused.
    async fn run<T>(
        &mut self,
        path: Option<&RemotePath>,
        what: &'static str,
        command: impl for<'a> FnOnce(&'a mut Stream) -> BoxFuture<'a, FtpResult<T>>,
    ) -> Result<T> {
        let Some(session) = self.session.as_mut() else {
            return Err(Error::Disconnected);
        };
        let result =
            tokio::time::timeout(self.pool.options.command_timeout, command(&mut session.ftp))
                .await;
        self.settle(result, path, what)
    }

    fn settle<T>(
        &mut self,
        result: std::result::Result<FtpResult<T>, tokio::time::error::Elapsed>,
        path: Option<&RemotePath>,
        what: &'static str,
    ) -> Result<T> {
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                let (error, broken) = ftp_error(error, path, what);
                self.broken |= broken;
                Err(error)
            }
            Err(_) => {
                self.broken = true;
                Err(Error::Timeout(what))
            }
        }
    }

    /// Closes a data connection and reads the server's verdict on it.
    pub(super) async fn finish(
        &mut self,
        transfer: Transfer,
        path: Option<&RemotePath>,
    ) -> Result<()> {
        let result =
            tokio::time::timeout(self.pool.options.command_timeout, transfer.finish()).await;
        self.settle(result, path, "transfer")
    }

    pub(super) async fn cwd(&mut self, path: &RemotePath) -> Result<()> {
        let target = path.as_str().to_owned();
        self.run(Some(path), "CWD", move |ftp| {
            Box::pin(async move { ftp.cwd(&target).await })
        })
        .await
    }

    pub(super) async fn pwd(&mut self) -> Result<RemotePath> {
        let path = self
            .run(None, "PWD", |ftp| Box::pin(async move { ftp.pwd().await }))
            .await?;
        RemotePath::parse(&path)
            .map_err(|_| Error::protocol("PWD", format!("{path:?} is not an absolute path")))
    }

    pub(super) async fn size(&mut self, path: &RemotePath) -> Result<u64> {
        let target = path.as_str().to_owned();
        self.run(Some(path), "SIZE", move |ftp| {
            Box::pin(async move {
                let reply = ftp
                    .custom_command(format!("SIZE {target}"), &[Status::File])
                    .await?;
                Ok(reply.as_string().ok().as_deref().and_then(parse_size))
            })
        })
        .await?
        .ok_or_else(|| Error::protocol("SIZE", "the server sent no size"))
    }

    pub(super) async fn mdtm(&mut self, path: &RemotePath) -> Result<SystemTime> {
        let target = path.as_str().to_owned();
        let reply = self
            .run(Some(path), "MDTM", move |ftp| {
                Box::pin(async move {
                    ftp.custom_command(format!("MDTM {target}"), &[Status::File])
                        .await
                })
            })
            .await?;
        reply
            .as_string()
            .ok()
            .and_then(|text| text.split_whitespace().nth(1).and_then(listing::time))
            .ok_or_else(|| Error::protocol("MDTM", "the server sent no time"))
    }

    pub(super) async fn mlst(&mut self, path: &RemotePath) -> Result<String> {
        let target = path.as_str().to_owned();
        self.run(Some(path), "MLST", move |ftp| {
            Box::pin(async move { ftp.mlst(Some(&target)).await })
        })
        .await
    }

    /// Lists the current folder (see [`Lease::cwd`]).
    pub(super) async fn list(&mut self, path: &RemotePath, format: Format) -> Result<Transfer> {
        let expect = [Status::AboutToSend, Status::AlreadyOpen];
        if format == Format::Mlsd {
            return self
                .run(Some(path), "MLSD", move |ftp| {
                    Box::pin(async move {
                        ftp.custom_data_command("MLSD", &expect)
                            .await
                            .map(|(_, t)| t)
                    })
                })
                .await;
        }
        if self.pool.list_hidden.load(Ordering::Relaxed) {
            match self
                .run(Some(path), "LIST", move |ftp| {
                    Box::pin(async move {
                        ftp.custom_data_command("LIST -a", &expect)
                            .await
                            .map(|(_, t)| t)
                    })
                })
                .await
            {
                Ok(transfer) => return Ok(transfer),
                Err(error) if super::is_refusal(&error) => {
                    tracing::debug!(%error, "the FTP server does not take LIST -a");
                    self.pool.list_hidden.store(false, Ordering::Relaxed);
                }
                Err(error) => return Err(error),
            }
        }
        self.run(Some(path), "LIST", move |ftp| {
            Box::pin(async move {
                ftp.custom_data_command("LIST", &expect)
                    .await
                    .map(|(_, t)| t)
            })
        })
        .await
    }

    pub(super) async fn retrieve(&mut self, path: &RemotePath, offset: u64) -> Result<Transfer> {
        let target = path.as_str().to_owned();
        let offset = usize::try_from(offset).map_err(|_| Error::Unsupported("files this large"))?;
        self.run(Some(path), "RETR", move |ftp| {
            Box::pin(async move {
                ftp.resume_transfer(offset).await?;
                ftp.retr_as_stream(&target).await
            })
        })
        .await
    }

    pub(super) async fn store(&mut self, path: &RemotePath, offset: u64) -> Result<Transfer> {
        let target = path.as_str().to_owned();
        let offset = usize::try_from(offset).map_err(|_| Error::Unsupported("files this large"))?;
        self.run(Some(path), "STOR", move |ftp| {
            Box::pin(async move {
                ftp.resume_transfer(offset).await?;
                ftp.put_with_stream(&target).await
            })
        })
        .await
    }

    pub(super) async fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let (source, target) = (from.as_str().to_owned(), to.as_str().to_owned());
        self.run(Some(from), "rename", move |ftp| {
            Box::pin(async move { ftp.rename(&source, &target).await })
        })
        .await
    }

    pub(super) async fn delete_file(&mut self, path: &RemotePath) -> Result<()> {
        let target = path.as_str().to_owned();
        self.run(Some(path), "DELE", move |ftp| {
            Box::pin(async move { ftp.rm(&target).await })
        })
        .await
    }

    pub(super) async fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
        let target = path.as_str().to_owned();
        self.run(Some(path), "RMD", move |ftp| {
            Box::pin(async move { ftp.rmdir(&target).await })
        })
        .await
    }

    pub(super) async fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        let target = path.as_str().to_owned();
        self.run(Some(path), "MKD", move |ftp| {
            Box::pin(async move { ftp.mkdir(&target).await })
        })
        .await
    }

    pub(super) async fn chmod(&mut self, path: &RemotePath, mode: u32) -> Result<()> {
        let command = format!("CHMOD {:o} {}", mode & 0o7777, path.as_str());
        self.run(Some(path), "SITE CHMOD", move |ftp| {
            Box::pin(async move { ftp.site(command).await.map(|_| ()) })
        })
        .await
    }

    /// Sets the modification time with `MFMT`, or else the way vsftpd and
    /// others take it, `MDTM` with a time.
    pub(super) async fn set_modified(
        &mut self,
        path: &RemotePath,
        time: SystemTime,
        mfmt: bool,
    ) -> Result<()> {
        let time = listing::format_time(time).ok_or(Error::Unsupported("times before 1970"))?;
        let verb = if mfmt { "MFMT" } else { "MDTM" };
        let command = format!("{verb} {time} {}", path.as_str());
        self.run(Some(path), verb, move |ftp| {
            Box::pin(async move {
                ftp.custom_command(command, &[Status::File])
                    .await
                    .map(|_| ())
            })
        })
        .await
    }
}

/// The first connection, secured: with TLS, after the questions about the
/// certificate, or without, if the server offers none and the user agrees.
async fn secure(options: &FtpOptions, interaction: &dyn Interaction) -> Result<(Stream, Security)> {
    let server = ServerId::new(&options.host, options.port());
    if options.tls == FtpTls::Implicit {
        let ((ftp, versions), client) = tls::establish(
            &server,
            &options.tls_options,
            interaction,
            |client| async move {
                preferring_resumable(|versions| open_implicit(options, &client, versions)).await
            },
        )
        .await?;
        return Ok((ftp, Security::Tls(client, versions)));
    }
    let ((outcome, versions), client) = tls::establish(
        &server,
        &options.tls_options,
        interaction,
        |client| async move {
            preferring_resumable(|versions| open_explicit(options, &client, versions)).await
        },
    )
    .await?;
    match outcome {
        Ok(ftp) => Ok((ftp, Security::Tls(client, versions))),
        Err(reason) => {
            tls::allow_plaintext(&server, "FTP", &reason, &options.tls_options, interaction)
                .await?;
            Ok((open_plain(options).await?, Security::Plain))
        }
    }
}

/// Logs in on `ftp`: anonymously, or with the given password and then
/// with what the user enters, a new connection for every attempt.
async fn log_in(
    ftp: Stream,
    options: &FtpOptions,
    security: &Security,
    interaction: &dyn Interaction,
) -> Result<(Stream, SecretString)> {
    let account = options.account();
    let failed = || Error::AuthenticationFailed {
        user: account.user.clone(),
        host: options.host.clone(),
    };
    if options.user.is_none() {
        let password = SecretString::from("anonymous@");
        let mut ftp = ftp;
        login(&mut ftp, options, &account.user, &password).await?;
        return Ok((ftp, password));
    }
    let mut ftp = Some(ftp);
    for attempt in 0..3 {
        let password = match (attempt, &options.password) {
            (0, Some(password)) => password.clone(),
            _ => interaction
                .password(&account, attempt > 0)
                .await
                .ok_or_else(failed)?,
        };
        let mut stream = match ftp.take() {
            Some(stream) => stream,
            None => open_with(options, security).await?,
        };
        match login(&mut stream, options, &account.user, &password).await {
            Ok(()) => return Ok((stream, password)),
            Err(error) if error.is_authentication() => {}
            Err(error) => return Err(error),
        }
    }
    Err(failed())
}

/// `future`, bounded by the command timeout.
async fn bounded<T>(
    options: &FtpOptions,
    what: &'static str,
    future: impl std::future::Future<Output = FtpResult<T>>,
) -> FtpResult<T> {
    match tokio::time::timeout(options.command_timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(FtpError::ConnectionError(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{what} timed out"),
        ))),
    }
}

/// The TCP connection to the server.
async fn dial(options: &FtpOptions) -> Result<TcpStream> {
    let target = ServerId::new(&options.host, options.port()).to_string();
    let connect = TcpStream::connect((options.host.as_str(), options.port()));
    let stream = match tokio::time::timeout(options.connect_timeout, connect).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(source)) => return Err(Error::Connect { target, source }),
        Err(_) => {
            return Err(Error::Connect {
                target,
                source: std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer"),
            });
        }
    };
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Connects without TLS and reads the greeting.
async fn open_plain(options: &FtpOptions) -> Result<Stream> {
    let tcp = dial(options).await?;
    bounded(options, "the greeting", Stream::connect_with_stream(tcp))
        .await
        .map_err(|error| ftp_error(error, None, "the greeting").0)
}

/// Connects and switches to TLS with `AUTH TLS`. `Ok(Err(reason))` if the
/// server does not offer it.
async fn open_explicit(
    options: &FtpOptions,
    client: &TlsClient,
    versions: Versions,
) -> Result<std::result::Result<Stream, String>> {
    let ftp = open_plain(options).await?;
    let connector = AsyncRustlsConnector::from(client.connector_of_its_own(versions));
    match bounded(
        options,
        "AUTH TLS",
        ftp.into_secure(connector, &options.host),
    )
    .await
    {
        Ok(ftp) => Ok(Ok(ftp)),
        Err(FtpError::UnexpectedResponse(reply)) => Ok(Err(format!(
            "it answered AUTH TLS with “{}”",
            reply_text(&reply).trim_end_matches('.')
        ))),
        Err(error) => Err(ftp_error(error, None, "the TLS handshake").0),
    }
}

/// Connects with TLS from the start.
async fn open_implicit(
    options: &FtpOptions,
    client: &TlsClient,
    versions: Versions,
) -> Result<Stream> {
    let connector = AsyncRustlsConnector::from(client.connector_of_its_own(versions));
    let address = (options.host.as_str(), options.port());
    let connect = Stream::connect_secure_implicit(address, connector, &options.host);
    let target = ServerId::new(&options.host, options.port()).to_string();
    match tokio::time::timeout(options.connect_timeout + options.command_timeout, connect).await {
        Ok(Ok(mut ftp)) => {
            let _ = ftp.get_ref().await.set_nodelay(true);
            // suppaftp protects data connections only after AUTH TLS; with
            // implicit TLS the server must be told as well.
            for command in ["PBSZ 0", "PROT P"] {
                bounded(
                    options,
                    "PROT P",
                    ftp.custom_command(command, &[Status::CommandOk]),
                )
                .await
                .map_err(|error| ftp_error(error, None, "PROT P").0)?;
            }
            Ok(ftp)
        }
        Ok(Err(FtpError::ConnectionError(source))) => Err(Error::Connect { target, source }),
        Ok(Err(error)) => Err(ftp_error(error, None, "the TLS handshake").0),
        Err(_) => Err(Error::Connect {
            target,
            source: std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer"),
        }),
    }
}

/// Opens the first connection with `open`, offering only TLS 1.2 so that
/// every data connection can resume the session (see
/// [`Versions::Resumable`]), and TLS 1.3 to a server that knows nothing
/// older.
async fn preferring_resumable<T, F, Fut>(mut open: F) -> Result<(T, Versions)>
where
    F: FnMut(Versions) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    match open(Versions::Resumable).await {
        Err(error) if tls::is_version_refusal(&error) => {
            tracing::debug!(%error, "the server knows no TLS 1.2, trying TLS 1.3");
            Ok((open(Versions::Any).await?, Versions::Any))
        }
        opened => Ok((opened?, Versions::Resumable)),
    }
}

/// Opens one more connection the way the first one was secured.
async fn open_with(options: &FtpOptions, security: &Security) -> Result<Stream> {
    match (security, options.tls) {
        (Security::Plain, _) => open_plain(options).await,
        (Security::Tls(client, versions), FtpTls::Implicit) => {
            open_implicit(options, client, *versions).await
        }
        (Security::Tls(client, versions), FtpTls::Explicit) => {
            match open_explicit(options, client, *versions).await? {
                Ok(ftp) => Ok(ftp),
                Err(reason) => Err(Error::protocol(
                    "AUTH TLS",
                    format!("the server stopped offering TLS: {reason}"),
                )),
            }
        }
    }
}

/// Logs in. A refused login is [`Error::AuthenticationFailed`].
async fn login(
    ftp: &mut Stream,
    options: &FtpOptions,
    user: &str,
    password: &SecretString,
) -> Result<()> {
    match bounded(
        options,
        "the login",
        ftp.login(user, password.expose_secret()),
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(FtpError::UnexpectedResponse(reply))
            if matches!(reply.status.code(), 530 | 430 | 331 | 332) =>
        {
            tracing::debug!(reply = %reply_text(&reply), "FTP login refused");
            Err(Error::AuthenticationFailed {
                user: user.to_owned(),
                host: options.host.clone(),
            })
        }
        Err(error) => Err(ftp_error(error, None, "the login").0),
    }
}

/// What the server offers. Missing answers count as "no".
async fn features(ftp: &mut Stream, options: &FtpOptions) -> Features {
    let feat = bounded(options, "FEAT", ftp.feat())
        .await
        .unwrap_or_default();
    let has = |name: &str| feat.keys().any(|key| key.eq_ignore_ascii_case(name));
    let rest = feat.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("REST")
            && value
                .as_deref()
                .is_some_and(|v| v.eq_ignore_ascii_case("STREAM"))
    });
    let system = bounded(options, "SYST", ftp.custom_command("SYST", &[Status::Name]))
        .await
        .map(|reply| reply_text(&reply))
        .unwrap_or_default();
    Features {
        mlst: has("MLST"),
        mfmt: has("MFMT"),
        rest,
        epsv: has("EPSV"),
        utf8: has("UTF8"),
        unix: !system.to_ascii_lowercase().contains("windows"),
    }
}

/// Binary transfers, UTF-8 names, passive mode with data connections that
/// time out.
async fn prepare(mut ftp: Stream, options: &FtpOptions, features: Features) -> Result<Stream> {
    bounded(options, "TYPE I", ftp.transfer_type(FileType::Binary))
        .await
        .map_err(|error| ftp_error(error, None, "TYPE I").0)?;
    if features.utf8 {
        let _ = bounded(options, "OPTS UTF8", ftp.opts("UTF8", Some("ON"))).await;
    }
    ftp.set_mode(if features.epsv {
        Mode::ExtendedPassive
    } else {
        Mode::Passive
    });
    // A server behind NAT announces its private address; the data
    // connection goes where the control connection goes.
    ftp.set_passive_nat_workaround(true);
    let timeout = options.connect_timeout;
    Ok(ftp.passive_stream_builder(move |address| {
        Box::pin(async move {
            match tokio::time::timeout(timeout, TcpStream::connect(address)).await {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    Ok(stream)
                }
                Ok(Err(error)) => Err(FtpError::ConnectionError(error)),
                Err(_) => Err(FtpError::ConnectionError(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the data connection timed out",
                ))),
            }
        })
    }))
}

/// The text of a reply, without its code.
fn reply_text(reply: &suppaftp::types::Response) -> String {
    let text = String::from_utf8_lossy(&reply.body);
    let text = text.trim();
    let code = reply.status.code().to_string();
    text.strip_prefix(code.as_str())
        .map_or(text, |rest| rest.trim_start_matches(['-', ' ']))
        .to_owned()
}

/// Maps an error of a command about `path`. The flag says whether the
/// connection is unusable afterwards.
pub(super) fn ftp_error(
    error: FtpError,
    path: Option<&RemotePath>,
    what: &'static str,
) -> (Error, bool) {
    match error {
        FtpError::ConnectionError(error) if error.kind() == std::io::ErrorKind::InvalidInput => (
            match path {
                Some(path) => Error::InvalidPath(path.as_str().into()),
                None => Error::protocol(what, error),
            },
            false,
        ),
        FtpError::ConnectionError(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            (Error::Timeout(what), true)
        }
        FtpError::ConnectionError(_) => (Error::Disconnected, true),
        FtpError::UnexpectedResponse(reply) => {
            let code = reply.status.code();
            let message = reply_text(&reply);
            // 421: the server closes the connection; 530 after the login:
            // the session ended.
            if code == 421 || code == 530 {
                return (Error::Disconnected, true);
            }
            let error = match path {
                Some(path) if message.to_ascii_lowercase().contains("permission denied") => {
                    Error::PermissionDenied { path: path.clone() }
                }
                Some(path) if is_path_refusal(reply.status) || code >= 500 => Error::Remote {
                    path: path.clone(),
                    message,
                },
                Some(path) => Error::Remote {
                    path: path.clone(),
                    message: format!("{what}: {code} {message}"),
                },
                None => Error::protocol(what, format!("{code} {message}")),
            };
            (error, false)
        }
        FtpError::SecureError(message) => (Error::protocol(what, message), true),
        FtpError::BadResponse => (
            Error::protocol(what, "the server sent a reply ferge does not understand"),
            true,
        ),
        FtpError::InvalidAddress(error) => (Error::protocol(what, error), true),
        FtpError::DataConnectionAlreadyOpen => (
            Error::protocol(what, "a data connection is still open"),
            true,
        ),
    }
}
