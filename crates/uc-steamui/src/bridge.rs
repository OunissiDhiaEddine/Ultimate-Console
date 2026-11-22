//! The bridge thread: finds Steam's UI, injects the menu script, answers the
//! tab's calls, and injects again when Steam reloads its UI.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cdp::{self, Session};
use crate::handler::{Handler, HandlerFactory};
use crate::protocol::{self, BINDING, Call, Health};
use crate::{Error, Result};

/// How long to wait between looks for Steam's DevTools port.
const NO_PORT_RETRY: Duration = Duration::from_secs(5);
/// Between looks for the shared UI context while Steam starts.
const NO_TARGET_RETRY: Duration = Duration::from_secs(3);
/// After a dropped connection.
const RECONNECT: Duration = Duration::from_secs(2);
/// How often a connected bridge checks that the script is still there.
const HEALTH_EVERY: Duration = Duration::from_secs(10);
/// Steam rebuilt its UI: wait a moment for it to load before injecting.
const REINJECT_AFTER: Duration = Duration::from_millis(1500);
/// How often `Handler::tick` runs.
const TICK: Duration = Duration::from_secs(1);
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// What the bridge needs from the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub port: u16,
}

impl From<&uc_core::Config> for Settings {
    fn from(c: &uc_core::Config) -> Self {
        Self {
            enabled: c.steam_menu.enabled,
            port: c.steam_menu.port,
        }
    }
}

/// Where the Quick Access Menu tab stands.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Status {
    /// Turned off in the settings.
    #[default]
    Off,
    /// Nothing listens on the DevTools port: Steam isn't running, or it was
    /// started before the switch file existed.
    NoDebugPort,
    /// Steam is up but its UI context isn't (yet), or another debugger has it.
    WaitingForSteamUi,
    /// The script runs in Steam; `health` is what it reports.
    Connected(Health),
    /// Something went wrong; retrying.
    Failed(String),
}

impl Status {
    /// The tab can be opened now.
    pub fn is_ready(&self) -> bool {
        matches!(self, Status::Connected(h) if h.hooked)
    }

    /// One line for the settings app and logs.
    pub fn text(&self) -> String {
        match self {
            Status::Off => "Off".into(),
            Status::NoDebugPort => {
                "Waiting for Steam (restart Steam once after turning this on)".into()
            }
            Status::WaitingForSteamUi => "Waiting for Steam's UI to start".into(),
            Status::Connected(h) if !h.hooked => {
                "Connected to Steam; waiting for Big Picture's Quick Access Menu".into()
            }
            Status::Connected(h) => {
                let missing = h.missing();
                let mut s = if h.tab_shown {
                    "Ready: the tab is in the Quick Access Menu".to_owned()
                } else {
                    "Ready: the tab shows the next time the Quick Access Menu opens".to_owned()
                };
                if !missing.is_empty() {
                    s.push_str(&format!(
                        " (simpler controls: {} not found)",
                        missing.join(", ")
                    ));
                }
                s
            }
            Status::Failed(e) => format!("Problem: {e}"),
        }
    }
}

enum Msg {
    Settings(Settings),
    Open(Option<Sender<bool>>),
    Push(String, Value),
    Eval(String, Sender<Result<Value>>),
    Stop,
}

/// Owner of the bridge thread. Dropping it stops the thread.
pub struct SteamMenu {
    tx: Sender<Msg>,
    status: Arc<Mutex<Status>>,
}

impl std::fmt::Debug for SteamMenu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamMenu")
            .field("status", &self.status())
            .finish()
    }
}

impl SteamMenu {
    /// Starts the bridge thread. `handler` is built on that thread (it may
    /// hold COM objects). `version` goes into the script, so a newer agent
    /// replaces an older script.
    pub fn start(
        settings: Settings,
        version: &str,
        handler: HandlerFactory,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::Off));
        let shared = status.clone();
        let script = protocol::menu_script(version);
        std::thread::Builder::new()
            .name("uc-steam-menu".into())
            .spawn(move || {
                let mut handler = handler();
                Bridge {
                    settings,
                    script,
                    status: shared,
                    rx,
                }
                .run(handler.as_mut());
            })?;
        Ok(Self { tx, status })
    }

    pub fn set_settings(&self, settings: Settings) {
        let _ = self.tx.send(Msg::Settings(settings));
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn is_ready(&self) -> bool {
        self.status().is_ready()
    }

    /// Opens the Quick Access Menu on the Ultimate Console tab and waits up
    /// to `timeout` to hear whether that worked.
    pub fn open(&self, timeout: Duration) -> bool {
        let (reply, answer) = mpsc::channel();
        if self.tx.send(Msg::Open(Some(reply))).is_err() {
            return false;
        }
        answer.recv_timeout(timeout).unwrap_or(false)
    }

    /// Asks an open tab to re-read its data now (e.g. after a volume key).
    pub fn refresh(&self) {
        let _ = self.tx.send(Msg::Push("refresh".into(), Value::Null));
    }

    /// Evaluates `expression` in Steam's UI context (for `uc-probe`).
    pub fn eval(&self, expression: &str, timeout: Duration) -> Result<Value> {
        let (reply, answer) = mpsc::channel();
        self.tx
            .send(Msg::Eval(expression.to_owned(), reply))
            .map_err(|_| Error::NotConnected)?;
        answer
            .recv_timeout(timeout)
            .map_err(|_| Error::Timeout("evaluate".into()))?
    }
}

impl Drop for SteamMenu {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// Why a session ended.
enum End {
    Stop,
    /// Settings changed: start over right away.
    Settings(Settings),
    /// Try again after this long.
    Retry(Duration),
}

struct Bridge {
    settings: Settings,
    script: String,
    status: Arc<Mutex<Status>>,
    rx: Receiver<Msg>,
}

impl Bridge {
    fn set(&self, status: Status) {
        if let Ok(mut s) = self.status.lock()
            && *s != status
        {
            let text = status.text();
            *s = status;
            tracing::info!("Steam menu: {text}");
        }
    }

    fn run(mut self, handler: &mut dyn Handler) {
        loop {
            if !self.settings.enabled {
                self.set(Status::Off);
                match self.rx.recv() {
                    Ok(Msg::Settings(s)) => self.settings = s,
                    Ok(Msg::Stop) | Err(_) => return,
                    Ok(other) => refuse(other),
                }
                continue;
            }
            let end = match self.connect() {
                Ok(session) => self.serve(session, handler),
                Err(end) => end,
            };
            match end {
                End::Stop => return,
                End::Settings(s) => self.settings = s,
                End::Retry(after) => {
                    if let Some(end) = self.idle(after, handler) {
                        match end {
                            End::Stop => return,
                            End::Settings(s) => self.settings = s,
                            End::Retry(_) => {}
                        }
                    }
                }
            }
        }
    }

    /// Waits `how_long` while disconnected, still answering messages.
    fn idle(&mut self, how_long: Duration, handler: &mut dyn Handler) -> Option<End> {
        let deadline = Instant::now() + how_long;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.rx.recv_timeout(left.min(TICK)) {
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return Some(End::Stop),
                Ok(Msg::Settings(s)) if s != self.settings => return Some(End::Settings(s)),
                Ok(Msg::Settings(_)) => {}
                Ok(other) => refuse(other),
                Err(RecvTimeoutError::Timeout) => handler.tick(),
            }
        }
    }

    fn connect(&self) -> Result<Session, End> {
        let port = self.settings.port;
        let targets = match cdp::list_targets(port, Duration::from_secs(1)) {
            Ok(t) => t,
            Err(Error::NoDebugPort { .. }) => {
                self.set(Status::NoDebugPort);
                return Err(End::Retry(NO_PORT_RETRY));
            }
            Err(e) => {
                self.set(Status::Failed(e.to_string()));
                return Err(End::Retry(NO_PORT_RETRY));
            }
        };
        let Some(target) = cdp::shared_context(&targets) else {
            self.set(Status::WaitingForSteamUi);
            return Err(End::Retry(NO_TARGET_RETRY));
        };
        let setup = || -> Result<Session> {
            let mut s = Session::connect(port, target, Duration::from_secs(5))?;
            s.call("Runtime.enable", json!({}), CALL_TIMEOUT)?;
            s.call(
                "Runtime.addBinding",
                json!({ "name": BINDING }),
                CALL_TIMEOUT,
            )?;
            Ok(s)
        };
        setup().map_err(|e| {
            self.set(Status::Failed(e.to_string()));
            End::Retry(RECONNECT)
        })
    }

    fn inject(&self, s: &mut Session) -> Result<()> {
        let value = s.evaluate(&self.script, CALL_TIMEOUT)?;
        s.clear_events();
        let health: Health = serde_json::from_value(value).unwrap_or_default();
        if !health.problems.is_empty() {
            tracing::warn!(problems = ?health.problems, "Steam menu script reported problems");
        }
        tracing::info!(found = ?health.found, how = ?health.how, "Steam menu script injected");
        self.set(Status::Connected(health));
        Ok(())
    }

    fn serve(&mut self, mut s: Session, handler: &mut dyn Handler) -> End {
        if let Err(e) = self.inject(&mut s) {
            self.set(Status::Failed(e.to_string()));
            return End::Retry(RECONNECT);
        }
        let mut reinject_at: Option<Instant> = None;
        let mut last_health = Instant::now();
        let mut last_tick = Instant::now();
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                match msg {
                    Msg::Stop => return End::Stop,
                    Msg::Settings(new) if new != self.settings => return End::Settings(new),
                    Msg::Settings(_) => {}
                    Msg::Open(reply) => {
                        let ok = s
                            .evaluate(protocol::OPEN_EXPRESSION, CALL_TIMEOUT)
                            .map(|v| v.as_bool().unwrap_or(false))
                            .unwrap_or(false);
                        if let Some(r) = reply {
                            let _ = r.send(ok);
                        }
                    }
                    Msg::Push(event, data) => {
                        let _ = s.evaluate_nowait(&protocol::push_expression(&event, &data));
                    }
                    Msg::Eval(expr, reply) => {
                        let _ = reply.send(s.evaluate(&expr, CALL_TIMEOUT));
                    }
                }
            }

            match s.next_event(Duration::from_millis(200)) {
                Ok(Some(event)) => {
                    if let Some(end) = self.on_event(&mut s, &event, handler, &mut reinject_at) {
                        return end;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::info!("Steam DevTools connection ended: {e}");
                    self.set(Status::WaitingForSteamUi);
                    return End::Retry(RECONNECT);
                }
            }

            if reinject_at.is_some_and(|at| Instant::now() >= at) {
                reinject_at = None;
                if let Err(e) = self.inject(&mut s) {
                    tracing::info!("injecting again failed: {e}");
                    reinject_at = Some(Instant::now() + REINJECT_AFTER);
                }
                last_health = Instant::now();
            }
            if last_health.elapsed() >= HEALTH_EVERY {
                last_health = Instant::now();
                match s.evaluate(protocol::HEALTH_EXPRESSION, CALL_TIMEOUT) {
                    Ok(Value::Null) => reinject_at = Some(Instant::now()),
                    Ok(v) => {
                        if let Ok(h) = serde_json::from_value::<Health>(v) {
                            self.set(Status::Connected(h));
                        }
                    }
                    Err(e @ (Error::Closed | Error::Io(_) | Error::WebSocket(_))) => {
                        tracing::info!("Steam DevTools connection lost: {e}");
                        return End::Retry(RECONNECT);
                    }
                    Err(e) => tracing::debug!("health check failed: {e}"),
                }
            }
            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                handler.tick();
            }
        }
    }

    fn on_event(
        &self,
        s: &mut Session,
        event: &Value,
        handler: &mut dyn Handler,
        reinject_at: &mut Option<Instant>,
    ) -> Option<End> {
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        let params = event.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "Runtime.bindingCalled" => {
                if params.get("name").and_then(Value::as_str) != Some(BINDING) {
                    return None;
                }
                let payload = params.get("payload").and_then(Value::as_str).unwrap_or("");
                match Call::parse(payload) {
                    Ok(call) => {
                        let result = handler.call(&call.method, &call.params);
                        if let Err(e) = &result {
                            tracing::info!(method = %call.method, "Steam menu call failed: {e}");
                        }
                        if call.wants_reply() {
                            let expr = protocol::reply_expression(call.id, &result);
                            if let Err(e) = s.evaluate_nowait(&expr) {
                                tracing::info!("could not answer the Steam menu: {e}");
                            }
                        }
                    }
                    Err(e) => tracing::warn!("bad message from the Steam menu: {e}"),
                }
                None
            }
            // Steam reloaded its UI (or one of its windows): the script is gone
            // from that context. Inject again once things settle; the script
            // itself is a no-op where it still runs.
            "Runtime.executionContextsCleared" | "Runtime.executionContextCreated" => {
                *reinject_at = Some(Instant::now() + REINJECT_AFTER);
                None
            }
            "Inspector.detached" | "Inspector.targetCrashed" => {
                tracing::info!(%method, "Steam UI target went away");
                self.set(Status::WaitingForSteamUi);
                Some(End::Retry(RECONNECT))
            }
            _ => None,
        }
    }
}

/// Answers a message that needs a connection while there is none.
fn refuse(msg: Msg) {
    match msg {
        Msg::Open(Some(r)) => {
            let _ = r.send(false);
        }
        Msg::Eval(_, r) => {
            let _ = r.send(Err(Error::NotConnected));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines() {
        assert!(!Status::NoDebugPort.is_ready());
        assert!(Status::NoDebugPort.text().contains("restart Steam"));
        let mut h = Health {
            hooked: true,
            ..Health::default()
        };
        assert!(Status::Connected(h.clone()).is_ready());
        assert!(Status::Connected(h.clone()).text().contains("next time"));
        h.tab_shown = true;
        h.found.insert("SliderField".into(), false);
        h.found.insert("Focusable".into(), true);
        let t = Status::Connected(h).text();
        assert!(t.contains("is in the Quick Access Menu"), "{t}");
        assert!(t.contains("SliderField not found"), "{t}");
        assert!(!Status::Connected(Health::default()).is_ready());
    }

    #[test]
    fn disabled_bridge_refuses_and_stops() {
        struct Nothing;
        impl Handler for Nothing {
            fn call(&mut self, _: &str, _: &Value) -> Result<Value, String> {
                Ok(Value::Null)
            }
        }
        let menu = SteamMenu::start(
            Settings {
                enabled: false,
                port: 1,
            },
            "test",
            Box::new(|| Box::new(Nothing) as Box<dyn Handler>),
        )
        .unwrap();
        assert!(!menu.open(Duration::from_secs(2)));
        assert!(matches!(
            menu.eval("1", Duration::from_secs(2)),
            Err(Error::NotConnected)
        ));
        assert_eq!(menu.status(), Status::Off);
    }
}
