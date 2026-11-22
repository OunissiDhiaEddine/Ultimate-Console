//! A small Chrome DevTools Protocol client for Steam's CEF: list the
//! targets (`GET /json`) and talk to one over its WebSocket.
//!
//! Only ever connects to `127.0.0.1`, whatever host the target list names.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

use crate::{Error, Result};

/// Title of the target that runs Steam's whole UI (Big Picture, the
/// overlay and the desktop client share it).
pub const SHARED_JS_CONTEXT: &str = "SharedJSContext";

/// Largest `/json` answer we accept.
const MAX_HTTP: usize = 4 << 20;

/// One debuggable page, as listed by `/json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Target {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub ws_url: Option<String>,
}

impl Target {
    /// Path part of the WebSocket URL (`/devtools/page/<id>`).
    pub fn ws_path(&self) -> Option<String> {
        let url = self.ws_url.as_deref()?;
        let rest = url
            .strip_prefix("ws://")
            .or_else(|| url.strip_prefix("wss://"))?;
        let slash = rest.find('/')?;
        Some(rest[slash..].to_owned())
    }
}

/// Steam's shared UI context. Another client (a second debugger) may hold
/// it, in which case `/json` lists it without a WebSocket URL.
pub fn shared_context(targets: &[Target]) -> Option<&Target> {
    targets
        .iter()
        .find(|t| t.title == SHARED_JS_CONTEXT && t.ws_path().is_some())
}

/// `GET http://127.0.0.1:<port>/json`.
pub fn list_targets(port: u16, timeout: Duration) -> Result<Vec<Target>> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut s = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|source| Error::NoDebugPort { port, source })?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    write!(
        s,
        "GET /json HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let raw = read_http(&mut s)?;
    let body = http_body(&raw)?;
    Ok(serde_json::from_slice(&body)?)
}

/// Reads one HTTP response: until the `Content-Length` is in, or EOF.
fn read_http(s: &mut impl Read) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(need) = complete_length(&buf)
            && buf.len() >= need
        {
            return Ok(buf);
        }
        match s.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
        if buf.len() > MAX_HTTP {
            return Err(Error::Http("answer too large".into()));
        }
    }
}

fn header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Total bytes of a response with a `Content-Length`, once the headers are in.
fn complete_length(raw: &[u8]) -> Option<usize> {
    let end = header_end(raw)?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let len: usize = header_value(head, "content-length")?.parse().ok()?;
    Some(end + len)
}

/// The body of a `200` response (plain or chunked).
fn http_body(raw: &[u8]) -> Result<Vec<u8>> {
    let end = header_end(raw).ok_or_else(|| Error::Http("no HTTP headers".into()))?;
    let head = std::str::from_utf8(&raw[..end]).map_err(|e| Error::Http(e.to_string()))?;
    let status = head.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(Error::Http(format!("unexpected status {status:?}")));
    }
    let body = &raw[end..];
    let chunked =
        header_value(head, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    if chunked {
        return dechunk(body);
    }
    Ok(match complete_length(raw) {
        Some(total) => raw[end..total.min(raw.len())].to_vec(),
        None => body.to_vec(),
    })
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let bad = || Error::Http("bad chunked body".into());
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n").ok_or_else(bad)?;
        let size_text = std::str::from_utf8(&body[..line_end]).map_err(|_| bad())?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| bad())?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err(bad());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

/// A DevTools session with one target.
pub struct Session {
    ws: WebSocket<TcpStream>,
    next_id: u64,
    /// Events read while waiting for an answer.
    events: VecDeque<Value>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Opens the target's WebSocket on `127.0.0.1:<port>`.
    pub fn connect(port: u16, target: &Target, timeout: Duration) -> Result<Self> {
        let path = target
            .ws_path()
            .ok_or_else(|| Error::Http(format!("target {:?} has no WebSocket", target.title)))?;
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .map_err(|source| Error::NoDebugPort { port, source })?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        let url = format!("ws://127.0.0.1:{port}{path}");
        let (ws, _) = tungstenite::client(url.as_str(), stream)
            .map_err(|e| Error::WebSocket(e.to_string()))?;
        Ok(Self {
            ws,
            next_id: 1,
            events: VecDeque::new(),
        })
    }

    /// Sends a command without waiting for its answer. Returns its id.
    pub fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "id": id, "method": method, "params": params });
        self.ws
            .send(Message::text(msg.to_string()))
            .map_err(ws_error)?;
        Ok(id)
    }

    /// Sends a command and waits for its answer (`result`). Events that
    /// arrive meanwhile are kept for [`Session::next_event`].
    pub fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.send(method, params)?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout(method.to_owned()));
            }
            let Some(msg) = self.read(left)? else {
                continue;
            };
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    let message = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_owned();
                    return Err(Error::Cdp {
                        method: method.to_owned(),
                        message,
                    });
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            if msg.get("method").is_some() {
                self.events.push_back(msg);
            }
            // Answers to fire-and-forget sends are dropped.
        }
    }

    /// `Runtime.evaluate` returning the value (JSON-able results only).
    pub fn evaluate(&mut self, expression: &str, timeout: Duration) -> Result<Value> {
        let r = self.call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true, "awaitPromise": false }),
            timeout,
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex
                .pointer("/exception/description")
                .or_else(|| ex.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("exception")
                .to_owned();
            return Err(Error::Script(text));
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    /// Evaluates without waiting (replies to the script).
    pub fn evaluate_nowait(&mut self, expression: &str) -> Result<()> {
        self.send(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )
        .map(drop)
    }

    /// The next event (`{"method": ..., "params": ...}`), waiting up to
    /// `timeout`. `Ok(None)` when nothing came.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<Value>> {
        if let Some(e) = self.events.pop_front() {
            return Ok(Some(e));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.read(left)? {
                Some(msg) if msg.get("method").is_some() => return Ok(Some(msg)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// Forgets events read so far (after injecting, the context events from
    /// `Runtime.enable` are old news).
    pub fn clear_events(&mut self) {
        self.events.clear();
    }

    /// One message, or `None` on timeout / non-text frames.
    fn read(&mut self, timeout: Duration) -> Result<Option<Value>> {
        self.ws
            .get_mut()
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        match self.ws.read() {
            Ok(Message::Text(t)) => Ok(Some(serde_json::from_str(t.as_str())?)),
            Ok(Message::Close(_)) => Err(Error::Closed),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                Ok(None)
            }
            Err(e) => Err(ws_error(e)),
        }
    }
}

fn ws_error(e: tungstenite::Error) -> Error {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => Error::Closed,
        tungstenite::Error::Io(io) => Error::Io(io),
        other => Error::WebSocket(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"[
      {"description":"","devtoolsFrontendUrl":"/devtools/inspector.html?ws=localhost:8080/devtools/page/AAA",
       "id":"AAA","title":"Steam Big Picture Mode","type":"page","url":"about:blank",
       "webSocketDebuggerUrl":"ws://localhost:8080/devtools/page/AAA"},
      {"id":"BBB","title":"SharedJSContext","type":"page",
       "url":"https://steamloopback.host/routes/",
       "webSocketDebuggerUrl":"ws://localhost:8080/devtools/page/BBB"}
    ]"#;

    #[test]
    fn finds_the_shared_context() {
        let targets: Vec<Target> = serde_json::from_str(LIST).unwrap();
        let t = shared_context(&targets).unwrap();
        assert_eq!(t.id, "BBB");
        assert_eq!(t.ws_path().as_deref(), Some("/devtools/page/BBB"));
    }

    #[test]
    fn a_target_held_by_another_debugger_is_skipped() {
        let targets = vec![Target {
            id: "X".into(),
            title: SHARED_JS_CONTEXT.into(),
            url: String::new(),
            kind: "page".into(),
            ws_url: None,
        }];
        assert!(shared_context(&targets).is_none());
    }

    #[test]
    fn http_with_content_length() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]";
        assert_eq!(complete_length(raw), Some(raw.len()));
        assert_eq!(http_body(raw).unwrap(), b"[]");
        // Not all in yet.
        assert!(complete_length(&raw[..raw.len() - 1]).unwrap() > raw.len() - 1);
    }

    #[test]
    fn http_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n[1,\r\n2\r\n2]\r\n0\r\n\r\n";
        assert_eq!(http_body(raw).unwrap(), b"[1,2]");
    }

    #[test]
    fn http_errors() {
        assert!(http_body(b"HTTP/1.1 404 Not Found\r\n\r\n").is_err());
        assert!(http_body(b"garbage").is_err());
    }

    #[test]
    fn read_http_stops_at_content_length() {
        // A server that keeps the connection open after the body.
        struct Slow(Vec<u8>, usize);
        impl Read for Slow {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.1 >= self.0.len() {
                    return Err(std::io::Error::new(ErrorKind::TimedOut, "would hang"));
                }
                let n = (self.0.len() - self.1).min(buf.len()).min(10);
                buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                self.1 += n;
                Ok(n)
            }
        }
        let body = LIST.as_bytes();
        let mut raw =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        raw.extend_from_slice(body);
        let got = read_http(&mut Slow(raw.clone(), 0)).unwrap();
        assert_eq!(got, raw);
    }
}
