//! A minimal ADB-over-TCP client: connect, RSA auth, `shell:` commands.
//!
//! This is the subset of the protocol in AOSP `packages/modules/adb/protocol.txt`
//! that the agent needs. No `adb.exe` or ADB server is involved (plan §2.6).

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};

use super::key::AdbKey;
use super::message::*;
use crate::{Result, TvError};

/// Most output we keep from one shell command.
const MAX_OUTPUT: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct ConnectOptions {
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Time allowed for each step of the handshake.
    pub io_timeout: Duration,
    /// If the TV does not know this PC's key yet: send the key (the TV shows
    /// "Allow USB debugging?") and wait this long for the user to accept.
    /// `None` fails right away with [`TvError::NotAuthorized`], which is what
    /// wake/sleep want, since nobody may be looking at the TV.
    pub pairing_wait: Option<Duration>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(2),
            io_timeout: Duration::from_secs(5),
            pairing_wait: None,
        }
    }
}

/// One authenticated connection to `adbd` on the TV.
#[derive(Debug)]
pub struct AdbConnection {
    stream: TcpStream,
    addr: SocketAddr,
    max_data: usize,
    next_local_id: u32,
    banner: String,
}

impl AdbConnection {
    pub fn connect(addr: SocketAddr, key: &AdbKey, opts: &ConnectOptions) -> Result<Self> {
        let stream = open_socket(addr, opts.connect_timeout)
            .map_err(|source| TvError::Connect { addr, source })?;
        let mut conn = Self {
            stream,
            addr,
            max_data: MAX_PAYLOAD as usize,
            next_local_id: 1,
            banner: String::new(),
        };
        conn.handshake(key, opts)?;
        tracing::debug!(%addr, banner = %conn.banner, "ADB connected");
        Ok(conn)
    }

    fn handshake(&mut self, key: &AdbKey, opts: &ConnectOptions) -> Result<()> {
        self.send(&Message::new(
            A_CNXN,
            A_VERSION,
            MAX_PAYLOAD,
            b"host::\0".to_vec(),
        ))?;

        let mut deadline = Instant::now() + opts.io_timeout;
        let mut sent_signature = false;
        let mut sent_public_key = false;
        loop {
            let msg = match self.recv(deadline, "the TV to accept the connection") {
                Err(TvError::Timeout(_)) if sent_public_key => {
                    return Err(TvError::NotAuthorized);
                }
                // adbd drops the socket if the key is refused on the TV.
                Err(TvError::Io(e)) if sent_public_key && is_disconnect(&e) => {
                    return Err(TvError::NotAuthorized);
                }
                other => other?,
            };
            match (msg.command, msg.arg0) {
                (A_CNXN, _) => {
                    self.max_data = (msg.arg1.clamp(4096, MAX_PAYLOAD)) as usize;
                    self.banner = String::from_utf8_lossy(&msg.data)
                        .trim_end_matches('\0')
                        .to_string();
                    return Ok(());
                }
                (A_AUTH, AUTH_TOKEN) if !sent_signature => {
                    let sig = key.sign_token(&msg.data)?;
                    self.send(&Message::new(A_AUTH, AUTH_SIGNATURE, 0, sig))?;
                    sent_signature = true;
                }
                // A second token means our signature was not accepted.
                (A_AUTH, AUTH_TOKEN) if !sent_public_key => {
                    let Some(wait) = opts.pairing_wait else {
                        return Err(TvError::NotAuthorized);
                    };
                    tracing::info!(addr = %self.addr, "TV does not know this PC yet; asking it to allow");
                    let mut pk = key.public_key().as_bytes().to_vec();
                    pk.push(0);
                    self.send(&Message::new(A_AUTH, AUTH_RSAPUBLICKEY, 0, pk))?;
                    sent_public_key = true;
                    deadline = Instant::now() + wait;
                }
                (A_AUTH, _) => return Err(TvError::NotAuthorized),
                (A_STLS, _) => return Err(TvError::TlsRequired),
                (cmd, _) => {
                    tracing::debug!(cmd = %command_name(cmd), "ignoring message during handshake");
                }
            }
        }
    }

    /// The device banner, e.g. `device::ro.product.name=…;ro.product.model=…;features=…`.
    pub fn banner(&self) -> &str {
        &self.banner
    }

    /// A property from the banner (`ro.product.model`, `ro.product.name`, …).
    pub fn banner_prop(&self, name: &str) -> Option<&str> {
        banner_prop(&self.banner, name)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Runs `cmd` through `shell:` and returns its output (stdout and stderr
    /// mixed, with `\r\n` normalised to `\n`). Any error leaves the connection
    /// in an unknown state; drop it and reconnect.
    pub fn shell(&mut self, cmd: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let local_id = self.next_local_id;
        self.next_local_id = self.next_local_id.wrapping_add(1).max(1);

        let mut service = format!("shell:{cmd}").into_bytes();
        service.push(0);
        if service.len() > self.max_data {
            return Err(TvError::Protocol("shell command too long".into()));
        }
        self.send(&Message::new(A_OPEN, local_id, 0, service))?;

        let mut remote_id = None;
        let mut output = Vec::new();
        loop {
            let msg = self.recv(deadline, "the TV to run a command")?;
            // Messages for other (stale) streams are not ours.
            if msg.command != A_SYNC && msg.arg1 != local_id {
                continue;
            }
            match msg.command {
                A_OKAY => remote_id = Some(msg.arg0),
                A_WRTE => {
                    remote_id = Some(msg.arg0);
                    if output.len() < MAX_OUTPUT {
                        output.extend_from_slice(&msg.data);
                    }
                    self.send(&Message::new(A_OKAY, local_id, msg.arg0, Vec::new()))?;
                }
                A_CLSE => {
                    match remote_id {
                        Some(remote) => {
                            self.send(&Message::new(A_CLSE, local_id, remote, Vec::new()))?
                        }
                        // CLSE(0, local) straight after OPEN: service refused.
                        None => {
                            return Err(TvError::Protocol(format!(
                                "the TV refused to run {cmd:?}"
                            )));
                        }
                    }
                    let text = String::from_utf8_lossy(&output).replace("\r\n", "\n");
                    return Ok(text);
                }
                _ => {}
            }
        }
    }

    /// Breaks the socket, to test reconnect paths.
    #[cfg(test)]
    pub(crate) fn kill(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    fn send(&mut self, msg: &Message) -> Result<()> {
        // Writes can block too if the TV vanished; bound them.
        self.stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .and_then(|()| msg.write_to(&mut self.stream))
            .map_err(|e| TvError::from_io(e, "the TV to take data"))
    }

    fn recv(&mut self, deadline: Instant, what: &'static str) -> Result<Message> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(TvError::Timeout(what));
        }
        self.stream
            .set_read_timeout(Some(left))
            .and_then(|()| Message::read_from(&mut self.stream))
            .map_err(|e| TvError::from_io(e, what))
    }
}

fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

/// TCP connect with a timeout, Nagle off (commands are tiny) and TCP
/// keepalive on, so a TV that vanished while we hold a persistent connection
/// is noticed within about half a minute instead of hours.
fn open_socket(addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.connect_timeout(&addr.into(), timeout)?;
    socket.set_tcp_nodelay(true)?;
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5));
    socket.set_tcp_keepalive(&keepalive)?;
    Ok(socket.into())
}

pub(crate) fn banner_prop<'a>(banner: &'a str, name: &str) -> Option<&'a str> {
    let props = banner.split_once("::").map_or(banner, |(_, p)| p);
    props
        .split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::adb::key::tests::test_key;
    use rsa::{Pkcs1v15Sign, RsaPublicKey};
    use sha1::Sha1;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// How the fake TV treats our key.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Trust {
        Known,
        AcceptOnPrompt,
        RefuseOnPrompt,
        NoAuth,
        Tls,
    }

    /// A fake `adbd` on localhost. `respond` maps a shell command to its output.
    /// Records every command it ran.
    pub(crate) struct FakeTv {
        pub addr: SocketAddr,
        pub commands: Arc<Mutex<Vec<String>>>,
    }

    impl FakeTv {
        pub fn start(trust: Trust, respond: fn(&str) -> String) -> Self {
            Self::start_on(TcpListener::bind("127.0.0.1:0").unwrap(), trust, respond)
        }

        pub fn start_on(listener: TcpListener, trust: Trust, respond: fn(&str) -> String) -> Self {
            let addr = listener.local_addr().unwrap();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let log = commands.clone();
            let public = test_key().rsa_public();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let (log, public) = (log.clone(), public.clone());
                    thread::spawn(move || {
                        let _ = serve(stream, trust, &public, respond, &log);
                    });
                }
            });
            Self { addr, commands }
        }

        pub fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    fn serve(
        mut s: TcpStream,
        trust: Trust,
        public: &RsaPublicKey,
        respond: fn(&str) -> String,
        log: &Mutex<Vec<String>>,
    ) -> io::Result<()> {
        let banner =
            b"device::ro.product.name=iris_tv;ro.product.model=IRIS TV;features=shell_v2\0";
        let cnxn = Message::new(A_CNXN, A_VERSION, MAX_PAYLOAD, banner.to_vec());
        let token = [42u8; 20];
        let hello = Message::read_from(&mut s)?;
        assert_eq!(hello.command, A_CNXN);
        match trust {
            Trust::NoAuth => cnxn.write_to(&mut s)?,
            Trust::Tls => {
                Message::new(A_STLS, 0x0100_0000, 0, Vec::new()).write_to(&mut s)?;
                return Ok(());
            }
            _ => {
                Message::new(A_AUTH, AUTH_TOKEN, 0, token.to_vec()).write_to(&mut s)?;
                let sig = Message::read_from(&mut s)?;
                assert_eq!((sig.command, sig.arg0), (A_AUTH, AUTH_SIGNATURE));
                let valid = public
                    .verify(Pkcs1v15Sign::new::<Sha1>(), &token, &sig.data)
                    .is_ok();
                assert!(valid, "signature must verify against the key");
                if trust == Trust::Known {
                    cnxn.write_to(&mut s)?;
                } else {
                    // Pretend we don't know the key: ask again.
                    Message::new(A_AUTH, AUTH_TOKEN, 0, token.to_vec()).write_to(&mut s)?;
                    let pk = Message::read_from(&mut s)?;
                    assert_eq!((pk.command, pk.arg0), (A_AUTH, AUTH_RSAPUBLICKEY));
                    let mut want = test_key().public_key().as_bytes().to_vec();
                    want.push(0);
                    assert_eq!(pk.data, want);
                    if trust == Trust::RefuseOnPrompt {
                        return Ok(()); // drop the socket, like adbd does
                    }
                    cnxn.write_to(&mut s)?;
                }
            }
        }

        let mut remote = 100;
        loop {
            let open = Message::read_from(&mut s)?;
            if open.command != A_OPEN {
                continue;
            }
            let local = open.arg0;
            let service = String::from_utf8_lossy(&open.data)
                .trim_end_matches('\0')
                .to_string();
            let Some(cmd) = service.strip_prefix("shell:") else {
                Message::new(A_CLSE, 0, local, Vec::new()).write_to(&mut s)?;
                continue;
            };
            log.lock().unwrap().push(cmd.to_string());
            remote += 1;
            Message::new(A_OKAY, remote, local, Vec::new()).write_to(&mut s)?;
            let out = respond(cmd).replace('\n', "\r\n");
            // Send output in two chunks to exercise reassembly.
            let mid = out.len() / 2;
            for part in [&out[..mid], &out[mid..]] {
                if part.is_empty() {
                    continue;
                }
                Message::new(A_WRTE, remote, local, part.as_bytes().to_vec()).write_to(&mut s)?;
                let ack = Message::read_from(&mut s)?;
                assert_eq!((ack.command, ack.arg0, ack.arg1), (A_OKAY, local, remote));
            }
            Message::new(A_CLSE, remote, local, Vec::new()).write_to(&mut s)?;
            let close = Message::read_from(&mut s)?;
            assert_eq!(
                (close.command, close.arg0, close.arg1),
                (A_CLSE, local, remote)
            );
        }
    }

    pub(crate) fn echo(cmd: &str) -> String {
        format!("ran {cmd}\n")
    }

    fn opts() -> ConnectOptions {
        ConnectOptions {
            pairing_wait: None,
            ..ConnectOptions::default()
        }
    }

    #[test]
    fn known_key_connects_and_runs_commands() {
        let tv = FakeTv::start(Trust::Known, echo);
        let mut c = AdbConnection::connect(tv.addr, test_key(), &opts()).unwrap();
        assert_eq!(c.banner_prop("ro.product.model"), Some("IRIS TV"));
        let t = Duration::from_secs(5);
        assert_eq!(c.shell("echo hi", t).unwrap(), "ran echo hi\n");
        assert_eq!(c.shell("second", t).unwrap(), "ran second\n");
        assert_eq!(tv.commands(), ["echo hi", "second"]);
    }

    #[test]
    fn empty_output() {
        let tv = FakeTv::start(Trust::Known, |_| String::new());
        let mut c = AdbConnection::connect(tv.addr, test_key(), &opts()).unwrap();
        assert_eq!(c.shell("true", Duration::from_secs(5)).unwrap(), "");
    }

    #[test]
    fn unknown_key_without_pairing_is_not_authorized() {
        let tv = FakeTv::start(Trust::AcceptOnPrompt, echo);
        let err = AdbConnection::connect(tv.addr, test_key(), &opts()).unwrap_err();
        assert!(matches!(err, TvError::NotAuthorized), "{err}");
    }

    #[test]
    fn pairing_sends_public_key() {
        let tv = FakeTv::start(Trust::AcceptOnPrompt, echo);
        let o = ConnectOptions {
            pairing_wait: Some(Duration::from_secs(5)),
            ..opts()
        };
        let mut c = AdbConnection::connect(tv.addr, test_key(), &o).unwrap();
        assert_eq!(c.shell("x", Duration::from_secs(5)).unwrap(), "ran x\n");
    }

    #[test]
    fn refused_prompt_is_not_authorized() {
        let tv = FakeTv::start(Trust::RefuseOnPrompt, echo);
        let o = ConnectOptions {
            pairing_wait: Some(Duration::from_secs(5)),
            ..opts()
        };
        let err = AdbConnection::connect(tv.addr, test_key(), &o).unwrap_err();
        assert!(matches!(err, TvError::NotAuthorized), "{err}");
    }

    #[test]
    fn no_auth_and_tls() {
        let tv = FakeTv::start(Trust::NoAuth, echo);
        AdbConnection::connect(tv.addr, test_key(), &opts()).unwrap();
        let tv = FakeTv::start(Trust::Tls, echo);
        let err = AdbConnection::connect(tv.addr, test_key(), &opts()).unwrap_err();
        assert!(matches!(err, TvError::TlsRequired), "{err}");
    }

    #[test]
    fn refused_service() {
        let tv = FakeTv::start(Trust::Known, echo);
        let mut c = AdbConnection::connect(tv.addr, test_key(), &opts()).unwrap();
        // The fake only knows `shell:`; anything else is refused. Reach into
        // the stream to send a raw OPEN for another service.
        c.send(&Message::new(A_OPEN, 77, 0, b"sync:\0".to_vec()))
            .unwrap();
        let reply = c
            .recv(Instant::now() + Duration::from_secs(5), "test")
            .unwrap();
        assert_eq!((reply.command, reply.arg0, reply.arg1), (A_CLSE, 0, 77));
    }

    #[test]
    fn connect_refused_is_retryable() {
        // Bind then drop to get a port nobody listens on.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let err = AdbConnection::connect(addr, test_key(), &opts()).unwrap_err();
        assert!(matches!(err, TvError::Connect { .. }), "{err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn banner_props() {
        let b = "device::ro.product.name=a;ro.product.model=IRIS 43;features=x,y";
        assert_eq!(banner_prop(b, "ro.product.model"), Some("IRIS 43"));
        assert_eq!(banner_prop(b, "features"), Some("x,y"));
        assert_eq!(banner_prop(b, "nope"), None);
        assert_eq!(banner_prop("device::", "x"), None);
    }
}
