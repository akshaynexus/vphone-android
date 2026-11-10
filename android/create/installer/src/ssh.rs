//! An SSH client for the ramdisk's dropbear (root/alpine) over any byte stream,
//! with a blocking facade so step code stays synchronous.

use crate::active::ActiveInstant;
use crate::error::{Error, Result};
use crate::progress::{check_cancel, Progress};
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh::client::{self, Handle as ClientHandle};
use russh::keys::PublicKeyOrCertificate;
use russh::{cipher, mac, ChannelMsg, Disconnect, Preferred};
use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

/// The ramdisk's login, and the program that serves SFTP (dropbear has no SFTP
/// subsystem, so the server is run as a command).
pub const USER: &str = "root";
pub const PASSWORD: &str = "alpine";
pub const SFTP_SERVER: &str = "/usr/libexec/sftp-server";

/// The SFTP library limits each request on the system clock, which runs on through a paused
/// creation: a request the guest was stopped in would fail when it resumes. So the library's
/// limit is out of the way. A guest that stops answering is caught by what watches the VM itself:
/// a VM that exits closes this connection, and one that stops moving is stopped (the engine's
/// stall watch).
const SFTP_TIMEOUT_SECS: u64 = 30 * 24 * 3600;
/// How often a command's time limit is looked at.
const LIMIT_TICK: Duration = Duration::from_millis(500);

struct AcceptAny {
    negotiated: Arc<Mutex<String>>,
}

impl client::Handler for AcceptAny {
    type Error = russh::Error;

    async fn kex_done(
        &mut self,
        _shared_secret: Option<&[u8]>,
        names: &russh::Names,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        *self.negotiated.lock().unwrap() = format!(
            "cipher={} client_mac={} server_mac={}",
            names.cipher.as_ref(),
            names.client_mac.as_ref(),
            names.server_mac.as_ref()
        );
        Ok(())
    }

    async fn check_server_key(&mut self, _key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // The tools this replaces use StrictHostKeyChecking=no: the peer is a
        // ramdisk reached over the VM's own virtual USB.
        Ok(true)
    }
}

/// The guest computes these in software under TCG, so the cheapest come first. The rest are
/// russh's defaults, for a server that offers neither: hmac-sha1 is not among those, and is here
/// for a link that is the VM's own virtual USB.
const CIPHERS: &[cipher::Name] = &[
    cipher::AES_128_CTR,
    cipher::CHACHA20_POLY1305,
    cipher::AES_256_GCM,
    cipher::AES_256_CTR,
    cipher::AES_192_CTR,
];
const MACS: &[mac::Name] = &[
    mac::HMAC_SHA1,
    mac::HMAC_SHA512_ETM,
    mac::HMAC_SHA256_ETM,
    mac::HMAC_SHA512,
    mac::HMAC_SHA256,
];

fn preferred_algorithms() -> Preferred {
    Preferred {
        cipher: Cow::Borrowed(CIPHERS),
        mac: Cow::Borrowed(MACS),
        ..Preferred::DEFAULT
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
    negotiated: Arc<Mutex<String>>,
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
            // Keepalives go out, but going unanswered does not end the session: a paused guest
            // answers none, for as long as the person keeps it paused.
            keepalive_max: 0,
            preferred: preferred_algorithms(),
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
        let negotiated = Arc::new(Mutex::new(String::new()));
        let handler_negotiated = negotiated.clone();
        let client = runtime.block_on(async {
            let handler = AcceptAny { negotiated: handler_negotiated };
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
            negotiated,
        })
    }

    /// The cipher and MACs this session agreed on.
    pub fn negotiated(&self) -> String {
        self.negotiated.lock().unwrap().clone()
    }

    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.runtime.block_on(f)
    }

    /// Run a command and collect its output and exit status. `timeout` is counted on the active
    /// clock, which stops while the creation is paused.
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
                Some(limit) => {
                    let began = ActiveInstant::now();
                    tokio::pin!(collect);
                    loop {
                        tokio::select! {
                            output = &mut collect => break output,
                            _ = tokio::time::sleep(LIMIT_TICK) => {
                                if began.elapsed() >= limit {
                                    break Err(Error::Ssh(format!("timed out: {command}")));
                                }
                            }
                        }
                    }
                }
                None => collect.await,
            }
        })
    }

    /// Run a command that may take long and says how far it is on its standard error: each piece
    /// of that goes to `on_stderr` as it arrives (and is kept, for an error message). No time
    /// limit; a cancel closes the channel and returns `Cancelled`.
    pub fn exec_watching(&self, command: &str, on_stderr: &(dyn Fn(&[u8]) + Sync), progress: &dyn Progress) -> Result<Output> {
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
            let mut out = Output::default();
            let mut status = None;
            loop {
                tokio::select! {
                    msg = channel.wait() => match msg {
                        Some(ChannelMsg::Data { data }) => out.stdout.extend_from_slice(&data),
                        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                            on_stderr(&data);
                            // What is kept is for an error message: its end says why.
                            out.stderr.extend_from_slice(&data);
                            if out.stderr.len() > 64 << 10 {
                                let cut = out.stderr.len() - (32 << 10);
                                out.stderr.drain(..cut);
                            }
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status),
                        Some(ChannelMsg::Close) | None => break,
                        Some(_) => {}
                    },
                    _ = tokio::time::sleep(LIMIT_TICK) => {
                        if check_cancel(progress).is_err() {
                            let _ = channel.close().await;
                            return Err(Error::Cancelled);
                        }
                    }
                }
            }
            out.status = status.unwrap_or(255);
            Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::{Command, Stdio};

    #[test]
    fn the_cheap_algorithms_lead_and_the_defaults_remain_as_fallback() {
        let preferred = preferred_algorithms();
        assert_eq!(preferred.cipher[0], cipher::AES_128_CTR);
        assert_eq!(preferred.mac[0], mac::HMAC_SHA1);
        for name in Preferred::DEFAULT.cipher.iter() {
            assert!(preferred.cipher.contains(name), "{name:?} dropped");
        }
        for name in Preferred::DEFAULT.mac.iter() {
            assert!(preferred.mac.contains(name), "{name:?} dropped");
        }
    }

    /// Against the ramdisk's dropbear: built from source by the setup in
    /// `android/scripts/build-create-steps.sh`. The login is refused here (no preload shim), which
    /// is an encrypted round trip after the algorithms were agreed.
    #[test]
    fn dropbear_2016_74_agrees_on_aes128_ctr_and_hmac_sha1() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/create-steps");
        let base = root.join("dropbear/src");
        assert!(base.join("dropbear").exists(), "build dropbear 2016.74 first");
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        let dir = tempfile::Builder::new().prefix("neg").tempdir_in(root.join("tmp")).unwrap();
        let key = dir.path().join("host_key");
        let generated = Command::new(base.join("dropbearkey"))
            .args(["-t", "ecdsa", "-f"])
            .arg(&key)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(generated.success());
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut server = Command::new(base.join("dropbear"))
            .args(["-F", "-p", &format!("127.0.0.1:{port}"), "-r"])
            .arg(&key)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let negotiated = Arc::new(Mutex::new(String::new()));
        let seen = negotiated.clone();
        let runtime = Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let refused = runtime.block_on(async move {
            let config = Arc::new(client::Config {
                preferred: preferred_algorithms(),
                ..Default::default()
            });
            let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let mut client = client::connect_stream(config, stream, AcceptAny { negotiated: seen }).await.unwrap();
            !client.authenticate_password(USER, "not-the-password").await.unwrap().success()
        });
        let _ = server.kill();
        let _ = server.wait();
        assert!(refused, "the wrong password must be refused");
        let negotiated = negotiated.lock().unwrap().clone();
        println!("negotiated: {negotiated}");
        assert_eq!(negotiated, "cipher=aes128-ctr client_mac=hmac-sha1 server_mac=hmac-sha1");
    }
}
