//! In-process fake agent, for building and trying the UI without the real
//! one (`uc-settings --mock`) and for tests.
//!
//! It speaks the real JSON-lines protocol over in-memory pipes, so the client
//! code path is exactly the one used with the named pipe. Displays, audio
//! devices and the TV are made up; actions take a moment, like the real ones.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use uc_core::config::{DisplayId, DisplayMode};
use uc_core::ipc::{
    self, AgentStatus, AudioDeviceInfo, ClientFrame, DisplayInfo, ErrorCode, IpcEvent, IpcRequest,
    IpcResponse, PROTOCOL_VERSION, ServerFrame, TvCommand, TvDevice, TvPowerState, UpdateStatus,
};
use uc_core::{Config, Hotkey, Mode};

use crate::client::{Connection, Connector};

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// How long the fake "slow" actions take (entering gaming mode, pairing…).
const DELAY: Duration = Duration::from_millis(800);

#[derive(Default)]
struct State {
    config: Config,
    mode: Mode,
    tv: TvPowerState,
}

/// The fake agent. Shared by every connection made through [`MockConnector`].
#[derive(Default)]
pub struct MockAgent {
    state: Mutex<State>,
    subscribers: Mutex<Vec<SharedWriter>>,
}

impl MockAgent {
    pub fn new() -> Arc<Self> {
        let agent = Self::default();
        agent.state.lock().unwrap().tv = TvPowerState::NotConfigured;
        Arc::new(agent)
    }

    fn status(&self) -> AgentStatus {
        let s = self.state.lock().unwrap();
        AgentStatus {
            agent_version: uc_core::VERSION.into(),
            mode: s.mode,
            tv: s.tv,
            update_token_set: false,
            last_error: None,
        }
    }

    fn broadcast(&self, event: IpcEvent) {
        let line = ipc::encode_line(&ServerFrame::Event { event }).expect("event encodes");
        self.subscribers.lock().unwrap().retain(|w| {
            let mut w = w.lock().unwrap();
            w.write_all(line.as_bytes())
                .and_then(|()| w.flush())
                .is_ok()
        });
    }

    fn set_mode(&self, mode: Mode) {
        self.state.lock().unwrap().mode = mode;
        self.broadcast(IpcEvent::ModeChanged { mode });
    }

    /// Moves to `via` now and to `to` after [`DELAY`], like a real mode change.
    fn change_mode(self: &Arc<Self>, via: Mode, to: Mode) {
        self.set_mode(via);
        let agent = self.clone();
        thread::spawn(move || {
            thread::sleep(DELAY);
            agent.set_mode(to);
        });
    }

    fn set_tv(&self, tv: TvPowerState) {
        self.state.lock().unwrap().tv = tv;
        self.broadcast(IpcEvent::TvPowerChanged { state: tv });
    }

    fn handle(self: &Arc<Self>, request: IpcRequest, writer: &SharedWriter) -> IpcResponse {
        use IpcResponse as R;
        match request {
            IpcRequest::Hello { .. } => R::Hello {
                agent_version: uc_core::VERSION.into(),
                protocol: PROTOCOL_VERSION,
            },
            IpcRequest::GetStatus => R::Status(self.status()),
            IpcRequest::GetConfig => R::Config {
                config: Box::new(self.state.lock().unwrap().config.clone()),
            },
            IpcRequest::SetConfig { config } => {
                if let Err(e) = config.validate() {
                    return R::error(ErrorCode::InvalidConfig, e.to_string());
                }
                let tv_enabled = config.tv.enabled;
                self.state.lock().unwrap().config = *config;
                if !tv_enabled {
                    self.set_tv(TvPowerState::NotConfigured);
                } else if self.state.lock().unwrap().tv == TvPowerState::NotConfigured {
                    self.set_tv(TvPowerState::Asleep);
                }
                self.broadcast(IpcEvent::ConfigReloaded);
                R::Ok
            }
            IpcRequest::CheckHotkey { action, hotkey } => {
                let mut check = match hotkey.parse::<Hotkey>() {
                    Ok(h) => h.check(),
                    Err(e) => uc_core::hotkey::HotkeyCheck {
                        errors: vec![e.to_string()],
                        ..Default::default()
                    },
                };
                // The real agent runs the AltGr check on installed layouts here.
                check.needs_altgr_check = false;
                tracing::debug!(?action, "mock hotkey check");
                R::HotkeyCheck(check)
            }
            IpcRequest::EnterGaming => {
                let s = self.state.lock().unwrap();
                if s.config.display.target.is_none() {
                    return R::error(ErrorCode::NotConfigured, "pick the TV on the Display page");
                }
                if s.mode != Mode::Desktop {
                    return R::error(ErrorCode::Busy, "already in gaming mode");
                }
                drop(s);
                self.change_mode(Mode::Entering, Mode::Gaming);
                R::Ok
            }
            IpcRequest::ExitGaming => {
                if self.state.lock().unwrap().mode != Mode::Gaming {
                    return R::error(ErrorCode::Busy, "not in gaming mode");
                }
                self.change_mode(Mode::Exiting, Mode::Desktop);
                R::Ok
            }
            IpcRequest::PanicRestore => {
                self.set_mode(Mode::Desktop);
                R::Ok
            }
            IpcRequest::ListDisplays => R::Displays {
                displays: fake_displays(),
            },
            IpcRequest::IdentifyDisplays => R::Ok,
            IpcRequest::ListAudioDevices => R::AudioDevices {
                devices: vec![
                    AudioDeviceInfo {
                        id: "{0.0.0.00000000}.{speakers}".into(),
                        name: "Speakers (Realtek Audio)".into(),
                        is_default: true,
                    },
                    AudioDeviceInfo {
                        id: "{0.0.0.00000000}.{iris-hdmi}".into(),
                        name: "IRIS TV (NVIDIA High Definition Audio)".into(),
                        is_default: false,
                    },
                ],
            },
            IpcRequest::TvDiscover => {
                thread::sleep(DELAY);
                R::TvDevices {
                    devices: vec![TvDevice {
                        name: "IRIS Android TV".into(),
                        ip: [192, 168, 1, 50].into(),
                        port: 5555,
                    }],
                }
            }
            IpcRequest::TvPair { .. } => {
                thread::sleep(DELAY * 2);
                R::TvPaired {
                    mac: Some("A0:B1:C2:D3:E4:F5".into()),
                }
            }
            IpcRequest::TvCommand { command } => {
                if !self.state.lock().unwrap().config.tv.enabled {
                    return R::error(ErrorCode::NotConfigured, "TV control is off");
                }
                thread::sleep(DELAY / 2);
                let state = match command {
                    TvCommand::Wake => TvPowerState::Awake,
                    TvCommand::Sleep => TvPowerState::Asleep,
                    TvCommand::State => self.state.lock().unwrap().tv,
                };
                self.set_tv(state);
                R::TvPower { state }
            }
            IpcRequest::CheckForUpdates => {
                thread::sleep(DELAY);
                R::Update(UpdateStatus::UpToDate {
                    version: uc_core::VERSION.into(),
                })
            }
            IpcRequest::SetUpdateToken { .. } | IpcRequest::ClearUpdateToken => R::error(
                ErrorCode::Unsupported,
                "no token needed: the repo is public",
            ),
            IpcRequest::Subscribe => {
                self.subscribers.lock().unwrap().push(writer.clone());
                R::Ok
            }
            IpcRequest::Shutdown => R::Ok,
        }
    }

    /// Serves one connection until the client goes away.
    pub fn serve(self: &Arc<Self>, reader: impl Read, writer: impl Write + Send + 'static) {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let frame = match ipc::decode_line::<ClientFrame>(&line) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("mock agent: bad request {line:?}: {e}");
                    continue;
                }
            };
            let response = self.handle(frame.request, &writer);
            let out = ipc::encode_line(&ServerFrame::Response {
                id: frame.id,
                response,
            })
            .expect("response encodes");
            let mut w = writer.lock().unwrap();
            if w.write_all(out.as_bytes())
                .and_then(|()| w.flush())
                .is_err()
            {
                return;
            }
        }
    }
}

fn fake_displays() -> Vec<DisplayInfo> {
    let mode = |width, height, hz| DisplayMode {
        width,
        height,
        refresh_hz: Some(hz),
    };
    vec![
        DisplayInfo {
            id: DisplayId {
                device_path: r"\\?\DISPLAY#DEL41B3#5&1a2b3c4d&0&UID4352#{e6f07b5f}".into(),
                friendly_name: "DELL S2721DGF".into(),
                connector: "DisplayPort".into(),
            },
            index: 1,
            active: true,
            primary: true,
            current_mode: Some(mode(2560, 1440, 165.0)),
            modes: vec![mode(2560, 1440, 165.0), mode(1920, 1080, 60.0)],
            hdr_supported: false,
        },
        DisplayInfo {
            id: DisplayId {
                device_path: r"\\?\DISPLAY#IRS0055#5&1a2b3c4d&0&UID4353#{e6f07b5f}".into(),
                friendly_name: "IRIS TV".into(),
                connector: "HDMI".into(),
            },
            index: 2,
            active: false,
            primary: false,
            current_mode: None,
            modes: vec![
                mode(3840, 2160, 60.0),
                mode(3840, 2160, 30.0),
                mode(1920, 1080, 120.0),
                mode(1920, 1080, 60.0),
            ],
            hdr_supported: true,
        },
    ]
}

/// Connects to a [`MockAgent`] through a pair of in-memory pipes.
pub struct MockConnector(Arc<MockAgent>);

impl MockConnector {
    pub fn new(agent: Arc<MockAgent>) -> Self {
        Self(agent)
    }
}

impl Connector for MockConnector {
    fn connect(&self) -> io::Result<Connection> {
        let (server_r, client_w) = io::pipe()?;
        let (client_r, server_w) = io::pipe()?;
        let agent = self.0.clone();
        thread::Builder::new()
            .name("mock-agent".into())
            .spawn(move || agent.serve(server_r, server_w))?;
        Ok(Connection::new(client_r, client_w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let mut c = MockConnector::new(MockAgent::new()).connect().unwrap();
        c.handshake().unwrap();
        c
    }

    #[test]
    fn invalid_config_is_refused() {
        let mut c = conn();
        let mut config = Config::default();
        config.hotkeys.overlay = config.hotkeys.gaming_toggle;
        match c
            .call(IpcRequest::SetConfig {
                config: Box::new(config),
            })
            .unwrap()
        {
            IpcResponse::Error { code, message } => {
                assert_eq!(code, ErrorCode::InvalidConfig);
                assert!(message.contains("same shortcut"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn gaming_needs_a_target_display() {
        let mut c = conn();
        assert!(matches!(
            c.call(IpcRequest::EnterGaming).unwrap(),
            IpcResponse::Error {
                code: ErrorCode::NotConfigured,
                ..
            }
        ));
        let mut config = Config::default();
        config.display.target = Some(fake_displays()[1].id.clone());
        let set = IpcRequest::SetConfig {
            config: Box::new(config),
        };
        assert_eq!(c.call(set).unwrap(), IpcResponse::Ok);
        assert_eq!(c.call(IpcRequest::EnterGaming).unwrap(), IpcResponse::Ok);
    }

    #[test]
    fn hotkey_check_reports_reserved() {
        let mut c = conn();
        let r = c
            .call(IpcRequest::CheckHotkey {
                action: ipc::HotkeyAction::Overlay,
                hotkey: "Alt+F4".into(),
            })
            .unwrap();
        let IpcResponse::HotkeyCheck(check) = r else {
            panic!("{r:?}")
        };
        assert!(!check.is_ok());
    }
}
