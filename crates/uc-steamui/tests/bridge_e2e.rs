//! The bridge against a fake Steam DevTools server on localhost: target
//! list over HTTP, then the WebSocket session (enable, binding, inject, a
//! call from the tab and the agent's reply).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::Message;
use uc_steamui::handler::HandlerFactory;
use uc_steamui::protocol::BINDING;
use uc_steamui::{Handler, Settings, Status, SteamMenu};

struct Echo;

impl Handler for Echo {
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        match method {
            "echo" => Ok(json!({ "got": params.clone() })),
            _ => Err(format!("unknown {method}")),
        }
    }
}

/// What the fake server saw.
#[derive(Debug)]
enum Seen {
    Method(String),
    Reply(String),
}

fn fake_steam(listener: TcpListener, seen: mpsc::Sender<Seen>) {
    let port = listener.local_addr().unwrap().port();
    // 1: GET /json
    let (mut http, _) = listener.accept().unwrap();
    let mut buf = [0u8; 2048];
    let n = http.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /json HTTP/1.1"));
    let body = json!([
        { "id": "A", "title": "Steam", "type": "page", "url": "about:blank",
          "webSocketDebuggerUrl": format!("ws://localhost:{port}/devtools/page/A") },
        { "id": "S", "title": "SharedJSContext", "type": "page",
          "url": "https://steamloopback.host/routes/",
          "webSocketDebuggerUrl": format!("ws://localhost:{port}/devtools/page/S") },
    ])
    .to_string();
    // Keep the connection open after the body, like Chromium may.
    write!(
        http,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();

    // 2: the WebSocket for the shared context.
    let (stream, _) = listener.accept().unwrap();
    // (The page path is checked by cdp's unit tests.)
    let mut ws = tungstenite::accept(stream).unwrap();
    let mut asked = false;
    loop {
        let msg = match ws.read() {
            Ok(Message::Text(t)) => serde_json::from_str::<Value>(t.as_str()).unwrap(),
            Ok(_) => continue,
            Err(_) => return,
        };
        let id = msg["id"].clone();
        let method = msg["method"].as_str().unwrap_or_default().to_owned();
        let result = match method.as_str() {
            "Runtime.addBinding" => {
                assert_eq!(msg["params"]["name"], BINDING);
                json!({})
            }
            "Runtime.evaluate" => {
                let expr = msg["params"]["expression"].as_str().unwrap_or_default();
                if expr.starts_with("window.__uc && window.__uc.reply(") {
                    let _ = seen.send(Seen::Reply(expr.to_owned()));
                    json!({ "result": { "type": "undefined" } })
                } else if expr.contains("const VERSION = 'e2e';") {
                    json!({ "result": { "type": "object", "value": {
                        "version": "e2e", "webpack": true, "react": true, "hooked": true,
                        "how": "QuickAccessMenu component", "tabShown": false,
                        "found": { "Focusable": true }, "problems": []
                    }}})
                } else {
                    json!({ "result": { "type": "undefined" } })
                }
            }
            _ => json!({}),
        };
        let _ = seen.send(Seen::Method(method.clone()));
        ws.send(Message::text(
            json!({ "id": id, "result": result }).to_string(),
        ))
        .unwrap();
        // After the injection, the "tab" calls the agent once.
        if method == "Runtime.evaluate" && !asked {
            asked = true;
            let payload = json!({ "id": 5, "method": "echo", "params": { "x": 1 } }).to_string();
            let event = json!({
                "method": "Runtime.bindingCalled",
                "params": { "name": BINDING, "payload": payload, "executionContextId": 1 }
            });
            ws.send(Message::text(event.to_string())).unwrap();
        }
    }
}

#[test]
fn injects_and_answers_a_call() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen) = mpsc::channel();
    std::thread::spawn(move || fake_steam(listener, seen_tx));

    let factory: HandlerFactory = Box::new(|| Box::new(Echo) as Box<dyn Handler>);
    let menu = SteamMenu::start(
        Settings {
            enabled: true,
            port,
        },
        "e2e",
        factory,
    )
    .unwrap();

    let mut methods = Vec::new();
    let reply = loop {
        match seen
            .recv_timeout(Duration::from_secs(10))
            .expect("fake Steam heard nothing")
        {
            Seen::Method(m) => methods.push(m),
            Seen::Reply(r) => break r,
        }
    };
    assert_eq!(
        &methods[..3],
        ["Runtime.enable", "Runtime.addBinding", "Runtime.evaluate"]
    );
    let json_part = reply
        .trim_start_matches("window.__uc && window.__uc.reply(")
        .trim_end_matches(')');
    let reply: Value = serde_json::from_str(json_part).unwrap();
    assert_eq!(reply, json!({ "id": 5, "result": { "got": { "x": 1 } } }));

    match menu.status() {
        Status::Connected(h) => {
            assert!(h.hooked);
            assert_eq!(h.found.get("Focusable"), Some(&true));
        }
        other => panic!("unexpected status {other:?}"),
    }
    assert!(menu.is_ready());
}

#[test]
fn nothing_listening_means_no_debug_port() {
    // Bind and drop to get a port that is (very likely) closed.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let factory: HandlerFactory = Box::new(|| Box::new(Echo) as Box<dyn Handler>);
    let menu = SteamMenu::start(
        Settings {
            enabled: true,
            port,
        },
        "e2e",
        factory,
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while menu.status() != Status::NoDebugPort {
        assert!(
            std::time::Instant::now() < deadline,
            "status {:?}",
            menu.status()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!menu.open(Duration::from_millis(200)));
}
