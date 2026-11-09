//! An SSH client for the ramdisk's dropbear (root/alpine) over any byte stream,
//! with a blocking facade so step code stays synchronous.

use crate::error::{Error, Result};
use crate::progress::{check_cancel, Progress};
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh::client::{self, Handle as ClientHandle};
use russh::keys::PublicKeyOrCertificate;
use russh::{ChannelMsg, Disconnect};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

/// The ramdisk's login, and the program that serves SFTP (dropbear has no SFTP
/// subsystem, so the server is run as a command).
pub const USER: &str = "root";
pub const PASSWORD: &str = "alpine";
pub const SFTP_SERVER: &str = "/usr/libexec/sftp-server";

/// The guest is emulated: a single request can take far longer than the library's default.
const SFTP_TIMEOUT_SECS: u64 = 600;

struct AcceptAny;

impl client::Handler for AcceptAny {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // The tools this replaces use StrictHostKeyChecking=no: the peer is a
        // ramdisk reached over the VM's own virtual USB.
        Ok(true)
    }
}

/// Where to reach sshd.
pub enum Target {
    /// A unix socket that is already a byte stream to the guest's port 22.
    Unix(PathBuf),
    /// Like `Unix`, for a socket made with `vusb_portfwd::connect`.
    Connected(std::os::unix::net::UnixStream),
    /// A TCP address (a local port forward).
    Tcp(String),
}

/// What `exec` got back.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub status: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
    pub fn check(self, what: &str) -> Result<Output> {
        if self.status == 0 {
            Ok(self)
        } else {
            Err(Error::Ssh(format!(
                "{what}: exit {}: {}",
                self.status,
                self.stderr_text()
            )))
        }
    }
}

/// A connected, authenticated session.
pub struct GuestSsh {
    runtime: Runtime,
    client: ClientHandle<AcceptAny>,
    sftp_program: String,
}

/// Options for tests and unusual hosts; defaults match the ramdisk.
pub struct Connect {
    pub target: Target,
    pub user: String,
    pub password: Option<String>,
    pub key: Option<Arc<russh::keys::PrivateKey>>,
    pub sftp_program: String,
    pub connect_timeout: Duration,
}

impl Connect {
    pub fn ramdisk(target: Target) -> Self {
        Self {
            target,
            user: USER.into(),
            password: Some(PASSWORD.into()),
            key: None,
            sftp_program: SFTP_SERVER.into(),
            connect_timeout: Duration::from_secs(60),
        }
    }
}

impl GuestSsh {
    /// Connect and authenticate.
    pub fn connect(options: Connect) -> Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| Error::io("tokio", e))?;
        let config = Arc::new(client::Config {
            keepalive_interval: Some(Duration::from_secs(15)),
            ..Default::default()
        });
        let timeout = options.connect_timeout;
        let Connect {
            target,
            user,
            password,
            key,
            sftp_program,
            ..
        } = options;
        let client = runtime.block_on(async {
            let handler = AcceptAny;
            let client = tokio::time::timeout(timeout, async {
                match target {
                    Target::Unix(path) => {
                        let stream = tokio::net::UnixStream::connect(&path)
                            .await
                            .map_err(|e| Error::Ssh(format!("connect {}: {e}", path.display())))?;
                        client::connect_stream(config, stream, handler)
                            .await
                            .map_err(|e| Error::Ssh(format!("handshake: {e:?}")))
                    }
                    Target::Connected(stream) => {
                        stream.set_nonblocking(true).map_err(|e| Error::io("socket", e))?;
                        let stream = tokio::net::UnixStream::from_std(stream).map_err(|e| Error::io("socket", e))?;
                        client::connect_stream(config, stream, handler)
                            .await
                            .map_err(|e| Error::Ssh(format!("handshake: {e:?}")))
                    }
                    Target::Tcp(addr) => {
                        let stream = tokio::net::TcpStream::connect(&addr)
                            .await
                            .map_err(|e| Error::Ssh(format!("connect {addr}: {e}")))?;
                        client::connect_stream(config, stream, handler)
                            .await
                            .map_err(|e| Error::Ssh(format!("handshake: {e:?}")))
                    }
                }
            })
            .await
            .map_err(|_| Error::Ssh("connect timed out".into()))??;
            let mut client = client;
            let authenticated = if let Some(key) = key {
                let hash = client
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| Error::Ssh(format!("{e:?}")))?
                    .flatten();
                client
                    .authenticate_publickey(user, russh::keys::PrivateKeyWithHashAlg::new(key, hash))
                    .await
                    .map_err(|e| Error::Ssh(format!("auth: {e:?}")))?
                    .success()
            } else if let Some(password) = password {
                client
                    .authenticate_password(user, password)
                    .await
                    .map_err(|e| Error::Ssh(format!("auth: {e:?}")))?
                    .success()
            } else {
                false
            };
            if !authenticated {
                return Err(Error::Ssh("authentication failed".into()));
            }
            Ok::<_, Error>(client)
        })?;
        Ok(Self {
            runtime,
            client,
            sftp_program,
        })
    }

    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.runtime.block_on(f)
    }

    /// Run a command and collect its output and exit status.
    pub fn exec(&self, command: &str, timeout: Option<Duration>) -> Result<Output> {
        self.block_on(async {
            let mut channel = self
                .client
                .channel_open_session()
                .await
                .map_err(|e| Error::Ssh(format!("channel: {e}")))?;
            channel
                .exec(true, command)
                .await
                .map_err(|e| Error::Ssh(format!("exec: {e}")))?;
            let collect = async {
                let mut out = Output::default();
                let mut status = None;
                while let Some(msg) = channel.wait().await {
                    match msg {
                        ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
                        ChannelMsg::ExtendedData { data, ext: 1 } => out.stderr.extend_from_slice(&data),
                        ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                        ChannelMsg::Close => break,
                        _ => {}
                    }
                }
                out.status = status.unwrap_or(255);
                Ok::<_, Error>(out)
            };
            match timeout {
                Some(t) => tokio::time::timeout(t, collect)
                    .await
                    .map_err(|_| Error::Ssh(format!("timed out: {command}")))?,
                None => collect.await,
            }
        })
    }

    async fn sftp_channel(&self) -> Result<russh::ChannelStream<client::Msg>> {
        let channel = self
            .client
            .channel_open_session()
            .await
            .map_err(|e| Error::Ssh(format!("channel: {e}")))?;
        channel
            .exec(true, self.sftp_program.as_str())
            .await
            .map_err(|e| Error::Ssh(format!("exec sftp-server: {e}")))?;
        Ok(channel.into_stream())
    }

    /// Open an SFTP session (the program runs as a command on its own channel).
    /// The second session is for `posix-rename@openssh.com`, which
    /// `SftpSession` does not expose.
    pub fn sftp(&self) -> Result<(SftpSession, RawSftpSession)> {
        self.block_on(async {
            let session = SftpSession::new(self.sftp_channel().await?)
                .await
                .map_err(|e| Error::Sftp(format!("init: {e}")))?;
            session.set_timeout(SFTP_TIMEOUT_SECS);
            let raw = RawSftpSession::new(self.sftp_channel().await?);
            raw.set_timeout(SFTP_TIMEOUT_SECS);
            raw.init()
                .await
                .map_err(|e| Error::Sftp(format!("init: {e}")))?;
            Ok((session, raw))
        })
    }

    /// Run `command` feeding it a byte stream produced on another thread. The
    /// bytes go through `chunks`, which applies backpressure to the producer.
    pub fn exec_with_stdin(
        &self,
        command: &str,
        mut chunks: mpsc::Receiver<Vec<u8>>,
        progress: &dyn Progress,
    ) -> Result<Output> {
        self.block_on(async {
            let channel = self
                .client
                .channel_open_session()
                .await
                .map_err(|e| Error::Ssh(format!("channel: {e}")))?;
            channel
                .exec(true, command)
                .await
                .map_err(|e| Error::Ssh(format!("exec: {e}")))?;
            let (mut read_half, write_half) = channel.split();
            let mut writer = write_half.make_writer();
            let pump = async {
                while let Some(chunk) = chunks.recv().await {
                    if check_cancel(progress).is_err() {
                        return Err(Error::Cancelled);
                    }
                    writer.write_all(&chunk).await.map_err(|e| Error::Ssh(format!("write: {e}")))?;
                }
                writer.shutdown().await.map_err(|e| Error::Ssh(format!("eof: {e}")))?;
                drop(writer);
                write_half.eof().await.map_err(|e| Error::Ssh(format!("eof: {e}")))?;
                Ok::<_, Error>(())
            };
            let collect = async {
                let mut out = Output::default();
                let mut status = None;
                while let Some(msg) = read_half.wait().await {
                    match msg {
                        ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
                        ChannelMsg::ExtendedData { data, ext: 1 } => out.stderr.extend_from_slice(&data),
                        ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                        ChannelMsg::Close => break,
                        _ => {}
                    }
                }
                out.status = status.unwrap_or(255);
                Ok::<_, Error>(out)
            };
            // A guest tar that stops early closes its channel while the pump is
            // still feeding it: its exit status then decides, not the pump.
            tokio::pin!(pump);
            tokio::pin!(collect);
            tokio::select! {
                biased;
                output = &mut collect => output,
                pumped = &mut pump => {
                    pumped?;
                    collect.await
                }
            }
        })
    }

    pub fn disconnect(self) {
        let client = self.client;
        self.runtime.block_on(async {
            let _ = client.disconnect(Disconnect::ByApplication, "", "").await;
        });
    }
}
