//! IPC between `uc-settings` (client) and `uc-agent` (server) (plan §2.10).
//!
//! Transport: named pipe [`crate::paths::pipe_name`], one JSON object per
//! line (UTF-8, `\n`-terminated). The client sends [`ClientFrame`]s; the agent
//! answers each with a [`ServerFrame::Response`] carrying the same `id`, and,
//! after [`IpcRequest::Subscribe`], also pushes [`ServerFrame::Event`]s.
//!
//! ```text
//! → {"id":1,"request":{"type":"get_status"}}
//! ← {"kind":"response","id":1,"response":{"type":"status","mode":"desktop",...}}
//! ← {"kind":"event","event":{"type":"mode_changed","mode":"gaming"}}
//! ```
//!
//! Compatibility: add variants and optional fields freely; bump
//! [`PROTOCOL_VERSION`] when changing or removing anything. Unknown request
//! types get [`ErrorCode::Unsupported`].

use std::net::IpAddr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::config::{Config, DisplayId, DisplayMode};
use crate::hotkey::HotkeyCheck;
use crate::state::Mode;

pub const PROTOCOL_VERSION: u32 = 1;

/// One line from client to agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    pub request: IpcRequest,
}

/// One line from agent to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerFrame {
    Response { id: u64, response: IpcResponse },
    Event { event: IpcEvent },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    /// First message on a connection. Answered with [`IpcResponse::Hello`].
    Hello {
        client_version: String,
    },
    GetStatus,
    GetConfig,
    /// Validate, save and apply. Answered with `Ok` or `Error{InvalidConfig}`.
    SetConfig {
        config: Box<Config>,
    },
    /// Full conflict check of a candidate shortcut for one action, including
    /// the OS checks (registration, AltGr on installed layouts).
    CheckHotkey {
        action: HotkeyAction,
        hotkey: String,
    },
    EnterGaming,
    ExitGaming,
    PanicRestore,
    ListDisplays,
    /// Flash a number on every display.
    IdentifyDisplays,
    ListAudioDevices,
    TvDiscover,
    TvPair {
        ip: IpAddr,
    },
    TvCommand {
        command: TvCommand,
    },
    CheckForUpdates,
    /// Stores the GitHub token for private-release updates (DPAPI-encrypted by the agent).
    SetUpdateToken {
        token: String,
    },
    ClearUpdateToken,
    /// Start receiving [`IpcEvent`]s on this connection.
    Subscribe,
    /// Stop the agent (used by the uninstaller). Restores desktop first if needed.
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyAction {
    GamingToggle,
    Overlay,
    PanicRestore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TvCommand {
    Wake,
    Sleep,
    State,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    Ok,
    Hello {
        agent_version: String,
        protocol: u32,
    },
    Status(AgentStatus),
    Config {
        config: Box<Config>,
    },
    HotkeyCheck(HotkeyCheck),
    Displays {
        displays: Vec<DisplayInfo>,
    },
    AudioDevices {
        devices: Vec<AudioDeviceInfo>,
    },
    TvDevices {
        devices: Vec<TvDevice>,
    },
    TvPaired {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mac: Option<String>,
    },
    TvPower {
        state: TvPowerState,
    },
    Update(UpdateStatus),
    Error {
        code: ErrorCode,
        message: String,
    },
}

impl IpcResponse {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Could not parse the request.
    InvalidRequest,
    /// `SetConfig` failed validation; the message lists the problems.
    InvalidConfig,
    /// Request type not known or not implemented yet.
    Unsupported,
    /// Another mode change is in progress.
    Busy,
    /// A needed setting is missing (e.g. no TV or no target display).
    NotConfigured,
    /// The action ran and failed; the message says why.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcEvent {
    ModeChanged {
        mode: Mode,
    },
    ConfigReloaded,
    TvPowerChanged {
        state: TvPowerState,
    },
    UpdateAvailable {
        version: String,
    },
    /// A user-visible problem (e.g. "TV display never appeared, gaming mode cancelled").
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent_version: String,
    pub mode: Mode,
    pub tv: TvPowerState,
    pub update_token_set: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TvPowerState {
    Awake,
    Asleep,
    /// Configured but not reachable over the network.
    Unreachable,
    /// TV control is disabled or not set up.
    NotConfigured,
    #[default]
    Unknown,
}

/// A display as listed by `uc-display` for the settings app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    /// Number shown by "Identify".
    pub index: u32,
    pub active: bool,
    pub primary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_mode: Option<DisplayMode>,
    #[serde(default)]
    pub modes: Vec<DisplayMode>,
    pub hdr_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    /// `IMMDevice::GetId`.
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// A TV found by mDNS (`_adb._tcp`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvDevice {
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdateStatus {
    UpToDate {
        version: String,
    },
    Available {
        version: String,
    },
    /// No GitHub token stored, so private releases can't be read.
    TokenNeeded,
    Failed {
        message: String,
    },
}

/// Serialises one frame as a JSON line (with the trailing `\n`).
pub fn encode_line<T: Serialize>(frame: &T) -> Result<String> {
    let mut s = serde_json::to_string(frame)?;
    s.push('\n');
    Ok(s)
}

/// Parses one JSON line (trailing whitespace allowed).
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T> {
    Ok(serde_json::from_str(line.trim_end())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_client(req: IpcRequest) {
        let f = ClientFrame {
            id: 7,
            request: req,
        };
        let line = encode_line(&f).unwrap();
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        assert_eq!(decode_line::<ClientFrame>(&line).unwrap(), f);
    }

    fn roundtrip_server(f: ServerFrame) {
        let line = encode_line(&f).unwrap();
        assert_eq!(decode_line::<ServerFrame>(&line).unwrap(), f);
    }

    #[test]
    fn all_requests_roundtrip() {
        for req in [
            IpcRequest::Hello {
                client_version: "0.1.0".into(),
            },
            IpcRequest::GetStatus,
            IpcRequest::GetConfig,
            IpcRequest::SetConfig {
                config: Box::default(),
            },
            IpcRequest::CheckHotkey {
                action: HotkeyAction::Overlay,
                hotkey: "Ctrl+Alt+O".into(),
            },
            IpcRequest::EnterGaming,
            IpcRequest::ExitGaming,
            IpcRequest::PanicRestore,
            IpcRequest::ListDisplays,
            IpcRequest::IdentifyDisplays,
            IpcRequest::ListAudioDevices,
            IpcRequest::TvDiscover,
            IpcRequest::TvPair {
                ip: "192.168.1.50".parse().unwrap(),
            },
            IpcRequest::TvCommand {
                command: TvCommand::Wake,
            },
            IpcRequest::CheckForUpdates,
            IpcRequest::SetUpdateToken {
                token: "github_pat_x".into(),
            },
            IpcRequest::ClearUpdateToken,
            IpcRequest::Subscribe,
            IpcRequest::Shutdown,
        ] {
            roundtrip_client(req);
        }
    }

    #[test]
    fn responses_and_events_roundtrip() {
        let responses = [
            IpcResponse::Ok,
            IpcResponse::Hello {
                agent_version: "0.1.0".into(),
                protocol: PROTOCOL_VERSION,
            },
            IpcResponse::Status(AgentStatus {
                agent_version: "0.1.0".into(),
                mode: Mode::Gaming,
                tv: TvPowerState::Awake,
                update_token_set: false,
                last_error: Some("x".into()),
            }),
            IpcResponse::Config {
                config: Box::default(),
            },
            IpcResponse::HotkeyCheck("Alt+Z".parse::<crate::Hotkey>().unwrap().check()),
            IpcResponse::Displays {
                displays: vec![DisplayInfo {
                    id: DisplayId::default(),
                    index: 1,
                    active: true,
                    primary: true,
                    current_mode: Some(DisplayMode {
                        width: 1920,
                        height: 1080,
                        refresh_hz: Some(60.0),
                    }),
                    modes: vec![],
                    hdr_supported: false,
                }],
            },
            IpcResponse::AudioDevices {
                devices: vec![AudioDeviceInfo {
                    id: "a".into(),
                    name: "TV".into(),
                    is_default: false,
                }],
            },
            IpcResponse::TvDevices {
                devices: vec![TvDevice {
                    name: "IRIS".into(),
                    ip: "10.0.0.2".parse().unwrap(),
                    port: 5555,
                }],
            },
            IpcResponse::TvPaired { mac: None },
            IpcResponse::TvPower {
                state: TvPowerState::Asleep,
            },
            IpcResponse::Update(UpdateStatus::TokenNeeded),
            IpcResponse::error(ErrorCode::Busy, "entering"),
        ];
        for (i, response) in responses.into_iter().enumerate() {
            roundtrip_server(ServerFrame::Response {
                id: i as u64,
                response,
            });
        }
        for event in [
            IpcEvent::ModeChanged {
                mode: Mode::Exiting,
            },
            IpcEvent::ConfigReloaded,
            IpcEvent::TvPowerChanged {
                state: TvPowerState::Unreachable,
            },
            IpcEvent::UpdateAvailable {
                version: "0.2.0".into(),
            },
            IpcEvent::Error {
                message: "x".into(),
            },
        ] {
            roundtrip_server(ServerFrame::Event { event });
        }
    }

    #[test]
    fn wire_format_examples() {
        let line = r#"{"id":1,"request":{"type":"get_status"}}"#;
        let f: ClientFrame = decode_line(line).unwrap();
        assert_eq!(f.request, IpcRequest::GetStatus);

        let line = r#"{"id":2,"request":{"type":"tv_command","command":"sleep"}}"#;
        let f: ClientFrame = decode_line(line).unwrap();
        assert_eq!(
            f.request,
            IpcRequest::TvCommand {
                command: TvCommand::Sleep
            }
        );

        let ev = ServerFrame::Event {
            event: IpcEvent::ModeChanged { mode: Mode::Gaming },
        };
        assert_eq!(
            encode_line(&ev).unwrap(),
            "{\"kind\":\"event\",\"event\":{\"type\":\"mode_changed\",\"mode\":\"gaming\"}}\n"
        );
    }

    #[test]
    fn unknown_request_is_an_error() {
        assert!(decode_line::<ClientFrame>(r#"{"id":1,"request":{"type":"fly"}}"#).is_err());
    }
}
