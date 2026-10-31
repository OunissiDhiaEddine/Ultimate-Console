//! IPC server for the settings app and the uninstaller (plan §2.10):
//! named pipe `\\.\pipe\UltimateConsole-<SID>`, JSON lines (`uc_core::ipc`).
//!
//! One thread per connection. Pipes are synchronous, so a connection that
//! sends `Subscribe` becomes event-only from then on: the agent stops
//! reading it and pushes [`IpcEvent`]s until the client goes away. That is
//! how `uc-settings` uses it (one connection for requests, one for events).
//!
//! Quick requests (status, lists, TV tests) are answered on the connection's
//! thread. Mode changes go to the orchestrator thread: enter/exit/panic are
//! answered `Ok` as soon as they are queued (progress arrives as events);
//! `SetConfig` and `Shutdown` wait for the result.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use serde::Serialize;
use uc_core::VERSION;
use uc_core::hotkey::{Hotkey, HotkeyCheck};
use uc_core::ipc::{
    self, AgentStatus, ClientFrame, ErrorCode, IpcEvent, IpcRequest, IpcResponse, PROTOCOL_VERSION,
    ServerFrame, TvCommand, TvDevice,
};
use uc_core::state::Mode;
use uc_tv::TvController;
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe,
};
use windows::core::HSTRING;

use crate::hotkeys;
use crate::platform::{SharedTv, tv_power_state};
use crate::service::Command;
use crate::shared::Shared;
use crate::updates::Updates;
use crate::window::WindowHandle;

const BUFFER: u32 = 64 * 1024;
/// How long `SetConfig` / `Shutdown` wait for the orchestrator, which may
/// be in the middle of entering gaming mode.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// How often an idle event connection is checked for a gone client.
const EVENT_IDLE_CHECK: Duration = Duration::from_secs(2);
const DISCOVER_FOR: Duration = Duration::from_secs(3);
/// How long pairing waits for "Allow" on the TV.
const PAIR_WAIT: Duration = Duration::from_secs(60);
const IDENTIFY_FOR: Duration = Duration::from_secs(3);

/// Answers requests. Cheap to clone, one per connection.
#[derive(Clone)]
pub struct Handler {
    pub shared: Arc<Shared>,
    pub tv: SharedTv,
    pub commands: Sender<Command>,
    pub window: WindowHandle,
    pub updates: Arc<Updates>,
}

impl Handler {
    pub fn handle(&self, request: IpcRequest) -> IpcResponse {
        use IpcRequest as Q;
        match request {
            Q::Hello { client_version } => {
                tracing::debug!(%client_version, "IPC hello");
                IpcResponse::Hello {
                    agent_version: VERSION.into(),
                    protocol: PROTOCOL_VERSION,
                }
            }
            Q::GetStatus => {
                let s = self.shared.status();
                IpcResponse::Status(AgentStatus {
                    agent_version: VERSION.into(),
                    mode: s.mode,
                    tv: s.tv,
                    update_token_set: self.updates.token_set(),
                    last_error: s.last_error,
                })
            }
            Q::GetConfig => IpcResponse::Config {
                config: Box::new(self.shared.config()),
            },
            Q::SetConfig { config } => self.wait_for(|reply| Command::SetConfig(config, reply)),
            Q::CheckHotkey { action, hotkey } => {
                IpcResponse::HotkeyCheck(match hotkey.parse::<Hotkey>() {
                    Err(e) => HotkeyCheck {
                        errors: vec![e.to_string()],
                        ..HotkeyCheck::default()
                    },
                    Ok(hk) => {
                        let altgr = hotkeys::altgr_clashes(hk);
                        let register_error = hk
                            .check()
                            .is_ok()
                            .then(|| self.window.probe_hotkey(hk))
                            .flatten();
                        hotkeys::full_check(
                            action,
                            hk,
                            &self.shared.config().hotkeys,
                            &altgr,
                            register_error,
                        )
                    }
                })
            }
            Q::EnterGaming => match self.shared.mode() {
                Mode::Desktop if self.shared.config().display.target.is_none() => {
                    IpcResponse::error(
                        ErrorCode::NotConfigured,
                        "pick the TV on the Display page first",
                    )
                }
                Mode::Desktop => self.queue(Command::Enter),
                mode => IpcResponse::error(ErrorCode::Busy, format!("agent is {mode:?}")),
            },
            Q::ExitGaming => match self.shared.mode() {
                Mode::Gaming => self.queue(Command::Exit),
                mode => {
                    IpcResponse::error(ErrorCode::Busy, format!("not in gaming mode ({mode:?})"))
                }
            },
            Q::PanicRestore => {
                self.shared.request_abort();
                self.queue(Command::Panic)
            }
            Q::ListDisplays => match uc_display::list() {
                Ok(displays) => IpcResponse::Displays { displays },
                Err(e) => failed(e),
            },
            Q::IdentifyDisplays => {
                std::thread::spawn(|| {
                    if let Err(e) = uc_display::identify(IDENTIFY_FOR) {
                        tracing::warn!("identify displays: {e}");
                    }
                });
                IpcResponse::Ok
            }
            Q::ListAudioDevices => {
                use uc_audio::AudioControl;
                match uc_audio::CoreAudio::new().and_then(|a| a.outputs()) {
                    Ok(devices) => IpcResponse::AudioDevices {
                        devices: devices.into_iter().map(Into::into).collect(),
                    },
                    Err(e) => failed(e),
                }
            }
            Q::TvDiscover => match uc_tv::mdns::discover(DISCOVER_FOR) {
                Ok(found) => IpcResponse::TvDevices {
                    devices: found
                        .into_iter()
                        .map(|d| TvDevice {
                            name: d.name,
                            ip: d.ip,
                            port: d.adb_port.unwrap_or(5555),
                        })
                        .collect(),
                },
                Err(e) => failed(e),
            },
            Q::TvPair { ip } => self.pair(ip),
            Q::TvCommand { command } => self.tv_command(command),
            Q::CheckForUpdates => self.updates.check_now(),
            Q::SetUpdateToken { token } => self.updates.set_token(&token),
            Q::ClearUpdateToken => self.updates.clear_token(),
            Q::Subscribe | Q::Shutdown => {
                unreachable!("handled by the connection loop")
            }
        }
    }

    fn queue(&self, cmd: Command) -> IpcResponse {
        match self.commands.send(cmd) {
            Ok(()) => IpcResponse::Ok,
            Err(_) => IpcResponse::error(ErrorCode::Failed, "the agent is shutting down"),
        }
    }

    fn wait_for(&self, make: impl FnOnce(Sender<IpcResponse>) -> Command) -> IpcResponse {
        let (tx, rx) = mpsc::channel();
        if self.commands.send(make(tx)).is_err() {
            return IpcResponse::error(ErrorCode::Failed, "the agent is shutting down");
        }
        rx.recv_timeout(COMMAND_TIMEOUT).unwrap_or_else(|_| {
            IpcResponse::error(ErrorCode::Busy, "the agent is busy; try again in a moment")
        })
    }

    /// Pairs with the TV at `ip` (the settings app saves the result).
    fn pair(&self, ip: std::net::IpAddr) -> IpcResponse {
        let key = match self.tv.key() {
            Ok(k) => k,
            Err(e) => return IpcResponse::error(ErrorCode::Failed, e),
        };
        let mut config = self.shared.config().tv;
        config.ip = Some(ip);
        let mut tv = TvController::new(config, key);
        match tv.pair(PAIR_WAIT) {
            Ok(info) => {
                if let Some(e) = &info.mac_error {
                    tracing::warn!("pairing: MAC not found: {e}");
                }
                IpcResponse::TvPaired { mac: info.mac }
            }
            Err(e) => failed(e),
        }
    }

    fn tv_command(&self, command: TvCommand) -> IpcResponse {
        if self.shared.config().tv.ip.is_none() {
            return IpcResponse::error(ErrorCode::NotConfigured, "set and save the TV's IP first");
        }
        let result = match command {
            TvCommand::Wake => self.tv.with(|tv| tv.wake()).map(|_| uc_tv::TvState::Awake),
            TvCommand::Sleep => self
                .tv
                .with(|tv| tv.sleep())
                .map(|()| uc_tv::TvState::Asleep),
            TvCommand::State => self.tv.with(|tv| tv.state()),
        };
        match result {
            Ok(state) => {
                let state = tv_power_state(&state);
                self.shared.set_tv(state);
                IpcResponse::TvPower { state }
            }
            Err(e) => IpcResponse::error(ErrorCode::Failed, e),
        }
    }
}

fn failed(e: impl std::fmt::Display) -> IpcResponse {
    IpcResponse::error(ErrorCode::Failed, e.to_string())
}

/// The answer to a line that is not a valid [`ClientFrame`]: `Unsupported`
/// for a well-formed request of an unknown type, else `InvalidRequest`.
pub fn bad_request(line: &str, error: &str) -> ServerFrame {
    let value: Option<serde_json::Value> = serde_json::from_str(line).ok();
    let id = value
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let request_type = value
        .as_ref()
        .and_then(|v| v.pointer("/request/type"))
        .and_then(|v| v.as_str());
    let response = match request_type {
        Some(t) => IpcResponse::error(ErrorCode::Unsupported, format!("unknown request {t:?}")),
        None => IpcResponse::error(ErrorCode::InvalidRequest, error),
    };
    ServerFrame::Response { id, response }
}

/// Starts the accept thread.
pub fn serve(handler: Handler) -> io::Result<()> {
    let name = uc_core::paths::pipe_name(&crate::sys::current_user_sid()?);
    tracing::info!(pipe = %name, "IPC server listening");
    std::thread::Builder::new()
        .name("uc-ipc-accept".into())
        .spawn(move || accept_loop(HSTRING::from(name), handler))?;
    Ok(())
}

fn accept_loop(name: HSTRING, handler: Handler) {
    let mut first = true;
    loop {
        let open_mode = if first {
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            PIPE_ACCESS_DUPLEX
        };
        // SAFETY: plain FFI call; the handle is owned by a File below.
        let pipe = unsafe {
            CreateNamedPipeW(
                &name,
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                None,
            )
        };
        if pipe.is_invalid() {
            tracing::error!(
                "CreateNamedPipe failed: {}; retrying",
                io::Error::last_os_error()
            );
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        first = false;
        // SAFETY: the handle is a fresh, valid pipe handle owned by nobody else.
        let file = unsafe { File::from_raw_handle(pipe.0) };
        // SAFETY: blocks until a client connects; `pipe` is valid.
        let connected = match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if !connected {
            continue;
        }
        let handler = handler.clone();
        let spawned = std::thread::Builder::new()
            .name("uc-ipc-conn".into())
            .spawn(move || {
                if let Err(e) = connection(file, &handler) {
                    tracing::debug!("IPC connection ended: {e}");
                }
            });
        if let Err(e) = spawned {
            tracing::error!("could not start an IPC connection thread: {e}");
        }
    }
}

fn send<T: Serialize>(out: &mut File, frame: &T) -> io::Result<()> {
    let line = ipc::encode_line(frame).map_err(io::Error::other)?;
    out.write_all(line.as_bytes())
}

fn connection(file: File, handler: &Handler) -> io::Result<()> {
    let mut out = file.try_clone()?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let frame: ClientFrame = match ipc::decode_line(&line) {
            Ok(f) => f,
            Err(e) => {
                send(&mut out, &bad_request(&line, &e.to_string()))?;
                continue;
            }
        };
        let id = frame.id;
        match frame.request {
            IpcRequest::Subscribe => {
                let events = handler.shared.subscribe();
                respond(&mut out, id, IpcResponse::Ok)?;
                return push_events(out, events);
            }
            IpcRequest::Shutdown => {
                let response = handler.wait_for(Command::Shutdown);
                let stopping = response == IpcResponse::Ok;
                respond(&mut out, id, response)?;
                // SAFETY: waits until the client has read the answer; `out`
                // is a valid pipe handle.
                let _ = unsafe { FlushFileBuffers(HANDLE(out.as_raw_handle())) };
                if stopping {
                    handler.window.close();
                }
                return Ok(());
            }
            request => {
                let response = handler.handle(request);
                respond(&mut out, id, response)?;
            }
        }
    }
    Ok(())
}

fn respond(out: &mut File, id: u64, response: IpcResponse) -> io::Result<()> {
    send(out, &ServerFrame::Response { id, response })
}

fn push_events(mut out: File, events: Receiver<IpcEvent>) -> io::Result<()> {
    loop {
        match events.recv_timeout(EVENT_IDLE_CHECK) {
            Ok(event) => send(&mut out, &ServerFrame::Event { event })?,
            Err(RecvTimeoutError::Timeout) => {
                // SAFETY: only asks whether the pipe is still connected.
                let alive = unsafe {
                    PeekNamedPipe(HANDLE(out.as_raw_handle()), None, 0, None, None, None)
                }
                .is_ok();
                if !alive {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_request_types_are_unsupported() {
        let line = r#"{"id":4,"request":{"type":"fly"}}"#;
        let ServerFrame::Response { id, response } = bad_request(line, "x") else {
            panic!()
        };
        assert_eq!(id, 4);
        assert!(matches!(
            response,
            IpcResponse::Error {
                code: ErrorCode::Unsupported,
                ..
            }
        ));
        let ServerFrame::Response { id, response } = bad_request("{nope", "bad json") else {
            panic!()
        };
        assert_eq!(id, 0);
        assert!(matches!(
            response,
            IpcResponse::Error {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ));
    }
}
