//! IPC client: talks to the agent over JSON lines (`uc_core::ipc`).
//!
//! Two connections run on two background threads so the UI never blocks:
//!
//! - **requests**: one request at a time, each answered by its response.
//! - **events**: `Hello` + `Subscribe`, then reads pushed events forever. It
//!   also tells the UI whether the agent is reachable, and reconnects every
//!   [`RETRY`] when it is not.
//!
//! Two connections instead of one because a synchronous named pipe handle
//! serialises reads and writes: a blocking read for events would hold up
//! every request written on the same handle.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use uc_core::ipc::{
    self, ClientFrame, IpcEvent, IpcRequest, IpcResponse, PROTOCOL_VERSION, ServerFrame,
};

/// Delay between reconnect attempts while the agent is unreachable.
pub const RETRY: Duration = Duration::from_secs(1);

/// Opens a new connection to the agent (named pipe, or the in-process mock).
pub trait Connector: Send + Sync + 'static {
    fn connect(&self) -> io::Result<Connection>;
}

/// One JSON-lines connection.
pub struct Connection {
    reader: Box<dyn BufRead + Send>,
    writer: Box<dyn Write + Send>,
    next_id: u64,
}

impl Connection {
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        Self {
            reader: Box::new(BufReader::new(reader)),
            writer: Box::new(writer),
            next_id: 1,
        }
    }

    fn send(&mut self, request: IpcRequest) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let line = ipc::encode_line(&ClientFrame { id, request }).map_err(invalid_data)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        Ok(id)
    }

    fn recv(&mut self) -> io::Result<ServerFrame> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the agent closed the connection",
            ));
        }
        ipc::decode_line(&line).map_err(invalid_data)
    }

    /// Sends a request and waits for its response. Events arriving in between
    /// are dropped (only the event connection subscribes).
    pub fn call(&mut self, request: IpcRequest) -> io::Result<IpcResponse> {
        let id = self.send(request)?;
        loop {
            match self.recv()? {
                ServerFrame::Response { id: got, response } if got == id => return Ok(response),
                ServerFrame::Response { id: got, .. } => {
                    tracing::warn!(expected = id, got, "dropping response with unexpected id");
                }
                ServerFrame::Event { .. } => {}
            }
        }
    }

    /// `Hello` exchange. Returns the agent's version.
    pub fn handshake(&mut self) -> io::Result<String> {
        let hello = IpcRequest::Hello {
            client_version: uc_core::VERSION.into(),
        };
        match self.call(hello)? {
            IpcResponse::Hello {
                agent_version,
                protocol,
            } if protocol == PROTOCOL_VERSION => Ok(agent_version),
            IpcResponse::Hello {
                agent_version,
                protocol,
            } => Err(io::Error::other(format!(
                "agent {agent_version} speaks protocol {protocol}, this app speaks \
                 {PROTOCOL_VERSION}; update both to the same version"
            ))),
            other => Err(invalid_data(format!(
                "unexpected answer to hello: {other:?}"
            ))),
        }
    }
}

fn invalid_data(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// What the background threads tell the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum FromAgent {
    Connected {
        agent_version: String,
    },
    Disconnected {
        reason: String,
    },
    /// Answer to a request sent with [`Client::send`]; carries the request
    /// so the UI knows what it was for.
    Reply {
        request: IpcRequest,
        result: Result<IpcResponse, String>,
    },
    Event(IpcEvent),
}

/// Handle used by the UI. Dropping it stops the request thread.
pub struct Client {
    requests: Sender<IpcRequest>,
    replies: Receiver<FromAgent>,
}

impl Client {
    /// Starts both threads. `wake` is called after every message so the UI
    /// can repaint (egui sleeps while nothing happens).
    pub fn start(connector: Arc<dyn Connector>, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<IpcRequest>();
        let (out_tx, out_rx) = mpsc::channel::<FromAgent>();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let notify = {
            let wake = wake.clone();
            move |tx: &Sender<FromAgent>, msg| {
                let sent = tx.send(msg).is_ok();
                wake();
                sent
            }
        };

        let (conn, tx, n) = (connector.clone(), out_tx.clone(), notify.clone());
        thread::Builder::new()
            .name("ipc-requests".into())
            .spawn(move || request_loop(&*conn, req_rx, |m| n(&tx, m)))
            .expect("spawn ipc request thread");

        thread::Builder::new()
            .name("ipc-events".into())
            .spawn(move || event_loop(&*connector, |m| notify(&out_tx, m)))
            .expect("spawn ipc event thread");

        Self {
            requests: req_tx,
            replies: out_rx,
        }
    }

    pub fn send(&self, request: IpcRequest) {
        // The thread only stops when `self` is dropped.
        let _ = self.requests.send(request);
    }

    /// Messages received since the last call.
    pub fn poll(&self) -> Vec<FromAgent> {
        self.replies.try_iter().collect()
    }
}

fn connect(connector: &dyn Connector) -> io::Result<(Connection, String)> {
    let mut conn = connector.connect()?;
    let version = conn.handshake()?;
    Ok((conn, version))
}

fn request_loop(
    connector: &dyn Connector,
    requests: Receiver<IpcRequest>,
    out: impl Fn(FromAgent) -> bool,
) {
    let mut conn: Option<Connection> = None;
    for request in requests {
        let result = (|| {
            if conn.is_none() {
                conn = Some(connect(connector)?.0);
            }
            let c = conn.as_mut().expect("connected above");
            c.call(request.clone()).inspect_err(|_| conn = None)
        })();
        let result = result.map_err(|e| {
            tracing::warn!(?request, "request failed: {e}");
            describe(&e)
        });
        if !out(FromAgent::Reply { request, result }) {
            return;
        }
    }
}

fn event_loop(connector: &dyn Connector, out: impl Fn(FromAgent) -> bool) {
    let mut last_reason: Option<String> = None;
    loop {
        let err = match connect(connector) {
            Ok((mut conn, agent_version)) => {
                last_reason = None;
                if !out(FromAgent::Connected { agent_version }) {
                    return;
                }
                read_events(&mut conn, &out)
            }
            Err(e) => e,
        };
        let reason = describe(&err);
        // Only report changes, so the UI isn't woken every second.
        if last_reason.as_deref() != Some(reason.as_str()) {
            tracing::info!("agent not reachable: {reason}");
            if !out(FromAgent::Disconnected {
                reason: reason.clone(),
            }) {
                return;
            }
            last_reason = Some(reason);
        }
        thread::sleep(RETRY);
    }
}

/// Subscribes and forwards events until the connection breaks. An agent that
/// can't push events is polled for its status instead, which still tells the
/// UI when it goes away.
fn read_events(conn: &mut Connection, out: &impl Fn(FromAgent) -> bool) -> io::Error {
    match conn.call(IpcRequest::Subscribe) {
        Ok(IpcResponse::Ok) => loop {
            match conn.recv() {
                Ok(ServerFrame::Event { event }) => {
                    if !out(FromAgent::Event(event)) {
                        return io::Error::other("ui closed");
                    }
                }
                Ok(ServerFrame::Response { .. }) => {}
                Err(e) => return e,
            }
        },
        Ok(other) => {
            tracing::warn!(?other, "agent refused subscribe; polling status instead");
            loop {
                thread::sleep(Duration::from_secs(2));
                match conn.call(IpcRequest::GetStatus) {
                    Ok(response) => {
                        let reply = FromAgent::Reply {
                            request: IpcRequest::GetStatus,
                            result: Ok(response),
                        };
                        if !out(reply) {
                            return io::Error::other("ui closed");
                        }
                    }
                    Err(e) => return e,
                }
            }
        }
        Err(e) => e,
    }
}

/// Short, user-facing text for a connection error.
fn describe(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => "the agent is not running".into(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockAgent, MockConnector};
    use std::time::Instant;
    use uc_core::Mode;

    fn wait_for(client: &Client, pred: impl Fn(&FromAgent) -> bool) -> FromAgent {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            for m in client.poll() {
                if pred(&m) {
                    return m;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for message");
    }

    #[test]
    fn handshake_and_call_over_lines() {
        let mut conn = MockConnector::new(MockAgent::new()).connect().unwrap();
        assert_eq!(conn.handshake().unwrap(), uc_core::VERSION);
        match conn.call(IpcRequest::GetStatus).unwrap() {
            IpcResponse::Status(s) => assert_eq!(s.mode, Mode::Desktop),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn client_connects_replies_and_receives_events() {
        let client = Client::start(Arc::new(MockConnector::new(MockAgent::new())), || {});
        wait_for(&client, |m| matches!(m, FromAgent::Connected { .. }));

        client.send(IpcRequest::GetConfig);
        let reply = wait_for(&client, |m| matches!(m, FromAgent::Reply { .. }));
        let FromAgent::Reply { request, result } = reply else {
            unreachable!()
        };
        assert_eq!(request, IpcRequest::GetConfig);
        assert!(matches!(result, Ok(IpcResponse::Config { .. })));

        // Saving the config makes the mock push `ConfigReloaded` to subscribers.
        client.send(IpcRequest::SetConfig {
            config: Box::default(),
        });
        wait_for(&client, |m| {
            matches!(m, FromAgent::Event(IpcEvent::ConfigReloaded))
        });
    }

    struct Refuse;
    impl Connector for Refuse {
        fn connect(&self) -> io::Result<Connection> {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    #[test]
    fn unreachable_agent_is_reported_once_and_requests_fail() {
        let client = Client::start(Arc::new(Refuse), || {});
        let m = wait_for(&client, |m| matches!(m, FromAgent::Disconnected { .. }));
        assert_eq!(
            m,
            FromAgent::Disconnected {
                reason: "the agent is not running".into()
            }
        );
        client.send(IpcRequest::GetStatus);
        let reply = wait_for(&client, |m| matches!(m, FromAgent::Reply { .. }));
        assert!(matches!(reply, FromAgent::Reply { result: Err(_), .. }));
        thread::sleep(RETRY * 2);
        assert!(
            !client
                .poll()
                .iter()
                .any(|m| matches!(m, FromAgent::Disconnected { .. })),
            "repeated disconnect"
        );
    }

    #[test]
    fn protocol_mismatch_is_an_error() {
        let (client_r, mut server_w) = io::pipe().unwrap();
        let (mut server_r, client_w) = io::pipe().unwrap();
        let server = thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(&mut server_r).read_line(&mut line).unwrap();
            let f: ClientFrame = ipc::decode_line(&line).unwrap();
            let reply = ServerFrame::Response {
                id: f.id,
                response: IpcResponse::Hello {
                    agent_version: "9.0.0".into(),
                    protocol: PROTOCOL_VERSION + 1,
                },
            };
            server_w
                .write_all(ipc::encode_line(&reply).unwrap().as_bytes())
                .unwrap();
        });
        let mut conn = Connection::new(client_r, client_w);
        let err = conn.handshake().unwrap_err();
        assert!(err.to_string().contains("protocol"), "{err}");
        server.join().unwrap();
    }
}
