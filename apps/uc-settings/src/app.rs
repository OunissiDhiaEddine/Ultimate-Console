//! App state, agent message handling, and the frame around the pages
//! (status header, page list, save bar). The pages are in `pages.rs`.

use std::collections::{HashMap, HashSet};
use std::mem::Discriminant;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use uc_core::config::{Hotkeys, parse_mac};
use uc_core::hotkey::HotkeyCheck;
use uc_core::ipc::{
    AgentStatus, AudioDeviceInfo, DisplayInfo, HotkeyAction, IpcEvent, IpcRequest, IpcResponse,
    TvDevice, TvPowerState, UpdateStatus,
};
use uc_core::{Config, Hotkey, Mode, Paths};

use crate::client::{Client, Connector, FromAgent};
use crate::keys::{self, Recorded};
use crate::system;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    General,
    Hotkeys,
    Display,
    Audio,
    Tv,
    Steam,
    Power,
    About,
}

impl Page {
    pub const ALL: [Page; 8] = [
        Page::General,
        Page::Hotkeys,
        Page::Display,
        Page::Audio,
        Page::Tv,
        Page::Steam,
        Page::Power,
        Page::About,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Hotkeys => "Hotkeys",
            Page::Display => "Display",
            Page::Audio => "Audio",
            Page::Tv => "TV",
            Page::Steam => "Steam",
            Page::Power => "Sleep / Resume",
            Page::About => "About / Updates",
        }
    }
}

pub enum AgentLink {
    Connecting,
    Connected { version: String },
    Down { reason: String },
}

/// Config fields edited as text. They are parsed into the draft every frame;
/// parse errors block saving.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fields {
    pub tv_ip: String,
    pub tv_mac: String,
    pub wol_address: String,
    pub after_wake: String,
    pub steam_exe: String,
    pub launch_args: String,
}

impl Fields {
    pub fn from_config(c: &Config) -> Self {
        Self {
            tv_ip: c.tv.ip.map(|ip| ip.to_string()).unwrap_or_default(),
            tv_mac: c.tv.mac.clone().unwrap_or_default(),
            wol_address: c.tv.wol_address.to_string(),
            after_wake: c.tv.after_wake_commands.join("\n"),
            steam_exe: c
                .steam
                .exe_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            launch_args: c.steam.launch_args.join("\n"),
        }
    }

    /// Writes the parsed values into `c`. Returns one message per bad field;
    /// those fields keep their previous value.
    pub fn apply(&self, c: &mut Config) -> Vec<String> {
        let mut errors = Vec::new();
        let ip = self.tv_ip.trim();
        if ip.is_empty() {
            c.tv.ip = None;
        } else {
            match ip.parse::<IpAddr>() {
                Ok(ip) => c.tv.ip = Some(ip),
                Err(_) => errors.push(format!("TV IP address: {ip:?} is not an IP address")),
            }
        }
        let mac = self.tv_mac.trim();
        if mac.is_empty() {
            c.tv.mac = None;
        } else if parse_mac(mac).is_some() {
            c.tv.mac = Some(mac.to_string());
        } else {
            errors.push(format!(
                "TV MAC: {mac:?} should look like AA:BB:CC:DD:EE:FF"
            ));
        }
        match self.wol_address.trim().parse::<IpAddr>() {
            Ok(a) => c.tv.wol_address = a,
            Err(_) => errors.push("Wake-on-LAN address must be an IP, e.g. 255.255.255.255".into()),
        }
        c.tv.after_wake_commands = lines(&self.after_wake);
        let exe = self.steam_exe.trim().trim_matches('"');
        c.steam.exe_path = (!exe.is_empty()).then(|| PathBuf::from(exe));
        c.steam.launch_args = lines(&self.launch_args);
        errors
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn hotkey_of(h: &Hotkeys, action: HotkeyAction) -> Hotkey {
    *hotkey_mut(&mut h.clone(), action)
}

pub fn hotkey_mut(h: &mut Hotkeys, action: HotkeyAction) -> &mut Hotkey {
    match action {
        HotkeyAction::GamingToggle => &mut h.gaming_toggle,
        HotkeyAction::Overlay => &mut h.overlay,
        HotkeyAction::PanicRestore => &mut h.panic_restore,
    }
}

pub const HOTKEY_ACTIONS: [(HotkeyAction, &str, &str); 3] = [
    (
        HotkeyAction::GamingToggle,
        "the gaming mode shortcut",
        "Gaming mode on / off",
    ),
    (
        HotkeyAction::Overlay,
        "the overlay shortcut",
        "Show / hide overlay",
    ),
    (
        HotkeyAction::PanicRestore,
        "the panic shortcut",
        "Panic restore (always back to desktop)",
    ),
];

pub struct App {
    client: Client,
    pub mock: bool,
    pub paths: Option<Paths>,
    pub link: AgentLink,
    pub status: Option<AgentStatus>,
    /// Config as the agent has it.
    pub saved: Option<Config>,
    /// Config being edited.
    pub draft: Option<Config>,
    pub fields: Fields,
    pub field_errors: Vec<String>,
    pub save_error: Option<String>,
    pub page: Page,
    /// Requests to send after this frame (pages can't borrow the client).
    pub outbox: Vec<IpcRequest>,
    /// Kinds of request waiting for an answer, to show spinners.
    pub pending: HashSet<Discriminant<IpcRequest>>,
    pub notice: Option<(bool, String)>,
    pub recording: Option<HotkeyAction>,
    /// Agent's verdict on a hotkey, keyed by action, for the shortcut it checked.
    pub hotkey_checks: HashMap<HotkeyAction, (String, HotkeyCheck)>,
    pub displays: Option<Vec<DisplayInfo>>,
    pub audio_devices: Option<Vec<AudioDeviceInfo>>,
    pub tv_devices: Option<Vec<TvDevice>>,
    pub pair_result: Option<Result<String, String>>,
    pub tv_test: Option<String>,
    pub update: Option<UpdateStatus>,
    pub token_input: String,
    pub confirm_uninstall: bool,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        connector: Arc<dyn Connector>,
        mock: bool,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        Self {
            client: Client::start(connector, move || ctx.request_repaint()),
            mock,
            paths: Paths::from_env().ok(),
            link: AgentLink::Connecting,
            status: None,
            saved: None,
            draft: None,
            fields: Fields::default(),
            field_errors: Vec::new(),
            save_error: None,
            page: Page::General,
            outbox: Vec::new(),
            pending: HashSet::new(),
            notice: None,
            recording: None,
            hotkey_checks: HashMap::new(),
            displays: None,
            audio_devices: None,
            tv_devices: None,
            pair_result: None,
            tv_test: None,
            update: None,
            token_input: String::new(),
            confirm_uninstall: false,
        }
    }

    pub fn connected(&self) -> bool {
        matches!(self.link, AgentLink::Connected { .. })
    }

    pub fn mode(&self) -> Option<Mode> {
        self.status.as_ref().map(|s| s.mode)
    }

    pub fn busy(&self, request: &IpcRequest) -> bool {
        self.pending.contains(&std::mem::discriminant(request))
    }

    pub fn dirty(&self) -> bool {
        self.draft.is_some() && self.draft != self.saved
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.notice = Some((true, text.into()));
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.notice = Some((false, text.into()));
    }

    fn flush(&mut self) {
        for request in self.outbox.drain(..) {
            self.pending.insert(std::mem::discriminant(&request));
            self.client.send(request);
        }
    }

    fn load_config(&mut self, config: Config) {
        self.fields = Fields::from_config(&config);
        for (action, _, _) in HOTKEY_ACTIONS {
            let hotkey = hotkey_of(&config.hotkeys, action).to_string();
            self.outbox.push(IpcRequest::CheckHotkey { action, hotkey });
        }
        self.draft = Some(config.clone());
        self.saved = Some(config);
        self.save_error = None;
    }

    pub fn revert(&mut self) {
        if let Some(saved) = self.saved.clone() {
            self.load_config(saved);
        }
    }

    pub fn save(&mut self) {
        if let Some(draft) = &self.draft {
            self.save_error = None;
            self.outbox.push(IpcRequest::SetConfig {
                config: Box::new(draft.clone()),
            });
        }
    }

    pub fn start_agent(&mut self) {
        match system::start_agent() {
            Ok(()) => self.info("Starting the agent…"),
            Err(e) => self.error(format!("Could not start the agent: {e}")),
        }
    }

    fn handle(&mut self, msg: FromAgent) {
        match msg {
            FromAgent::Connected { agent_version } => {
                tracing::info!(%agent_version, "connected to the agent");
                self.link = AgentLink::Connected {
                    version: agent_version,
                };
                self.outbox.push(IpcRequest::GetStatus);
                self.outbox.push(IpcRequest::GetConfig);
            }
            FromAgent::Disconnected { reason } => {
                self.link = AgentLink::Down { reason };
                self.status = None;
            }
            FromAgent::Reply { request, result } => {
                self.pending.remove(&std::mem::discriminant(&request));
                match result {
                    Ok(response) => self.handle_reply(request, response),
                    Err(e) => self.error(format!("{}: {e}", what(&request))),
                }
            }
            FromAgent::Event(event) => self.handle_event(event),
        }
    }

    fn handle_reply(&mut self, request: IpcRequest, response: IpcResponse) {
        use IpcRequest as Q;
        use IpcResponse as R;
        match (request, response) {
            (_, R::Status(s)) => self.status = Some(s),
            (_, R::Config { config }) => {
                if self.dirty() {
                    // Keep the user's edits; the next save overwrites.
                    self.saved = Some(*config);
                } else {
                    self.load_config(*config);
                }
            }
            (Q::SetConfig { config }, R::Ok) => {
                self.saved = Some(*config);
                self.info("Saved. The agent uses the new settings now.");
            }
            (Q::SetConfig { .. }, R::Error { message, .. }) => {
                self.save_error = Some(message);
            }
            (Q::CheckHotkey { action, hotkey }, R::HotkeyCheck(check)) => {
                self.hotkey_checks.insert(action, (hotkey, check));
            }
            (_, R::Displays { displays }) => self.displays = Some(displays),
            (_, R::AudioDevices { devices }) => self.audio_devices = Some(devices),
            (_, R::TvDevices { devices }) => {
                if devices.is_empty() {
                    self.info("No TV found. Check that network debugging is on, or type the IP.");
                }
                self.tv_devices = Some(devices);
            }
            (Q::TvPair { ip }, R::TvPaired { mac }) => {
                self.fields.tv_ip = ip.to_string();
                if let Some(mac) = &mac {
                    self.fields.tv_mac = mac.clone();
                }
                if let Some(d) = &mut self.draft {
                    d.tv.enabled = true;
                }
                self.pair_result = Some(Ok(match mac {
                    Some(mac) => format!("Paired with {ip} (MAC {mac}). Press Save to keep it."),
                    None => format!(
                        "Paired with {ip}, but its MAC wasn't found: type it in for Wake-on-LAN. \
                         Press Save to keep it."
                    ),
                }));
            }
            (Q::TvPair { .. }, R::Error { message, .. }) => {
                self.pair_result = Some(Err(message));
            }
            (Q::TvCommand { command }, R::TvPower { state }) => {
                self.tv_test = Some(format!("{command:?}: TV is {}", tv_name(state)));
                if let Some(s) = &mut self.status {
                    s.tv = state;
                }
            }
            (Q::TvCommand { command }, R::Error { message, .. }) => {
                self.tv_test = Some(format!("{command:?} failed: {message}"));
            }
            (_, R::Update(u)) => self.update = Some(u),
            (Q::SetUpdateToken { .. }, R::Ok) => {
                self.token_input.clear();
                self.info("Update token saved (encrypted for your Windows account).");
                self.outbox.push(IpcRequest::GetStatus);
            }
            (Q::ClearUpdateToken, R::Ok) => {
                self.info("Update token removed.");
                self.outbox.push(IpcRequest::GetStatus);
            }
            (Q::IdentifyDisplays, R::Ok) => {
                self.info("Each screen shows its number for a few seconds.");
            }
            (request, R::Error { code, message }) => {
                self.error(format!("{}: {message} ({code:?})", what(&request)));
            }
            (_, R::Ok) => {}
            (request, response) => {
                tracing::warn!(?request, ?response, "unexpected response");
            }
        }
    }

    fn handle_event(&mut self, event: IpcEvent) {
        match event {
            IpcEvent::ModeChanged { mode } => {
                if let Some(s) = &mut self.status {
                    s.mode = mode;
                }
            }
            IpcEvent::ConfigReloaded => self.outbox.push(IpcRequest::GetConfig),
            IpcEvent::TvPowerChanged { state } => {
                if let Some(s) = &mut self.status {
                    s.tv = state;
                }
            }
            IpcEvent::UpdateAvailable { version } => {
                self.update = Some(UpdateStatus::Available { version });
            }
            IpcEvent::Error { message } => {
                if let Some(s) = &mut self.status {
                    s.last_error = Some(message.clone());
                }
                self.error(message);
            }
        }
    }

    /// Feeds key presses to the shortcut recorder and hides them from the
    /// widgets (so Tab doesn't move focus while recording).
    fn record_hotkey(&mut self, ctx: &egui::Context) {
        let Some(action) = self.recording else {
            return;
        };
        let got = ctx.input_mut(|i| {
            let mut got = None;
            i.events.retain(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    got = got.or_else(|| keys::record(*key, *modifiers));
                    false
                }
                egui::Event::Key { .. } | egui::Event::Text(_) => false,
                _ => true,
            });
            got
        });
        match got {
            None => {}
            Some(Recorded::Cancel) => self.recording = None,
            Some(Recorded::Hotkey(h)) => {
                self.recording = None;
                if let Some(d) = &mut self.draft {
                    *hotkey_mut(&mut d.hotkeys, action) = h;
                }
                self.outbox.push(IpcRequest::CheckHotkey {
                    action,
                    hotkey: h.to_string(),
                });
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("Ultimate Console");
            if self.mock {
                ui.label(
                    egui::RichText::new("MOCK AGENT: nothing here touches the PC")
                        .color(ui.visuals().warn_fg_color)
                        .strong(),
                );
            }
        });
        match &self.link {
            AgentLink::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting to the agent…");
                });
            }
            AgentLink::Down { reason } => {
                let reason = reason.clone();
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("Agent not reachable ({reason})."),
                    );
                    ui.label("It normally starts with Windows.");
                    if ui.button("Start agent").clicked() {
                        self.start_agent();
                    }
                });
            }
            AgentLink::Connected { version } => {
                let version = version.clone();
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("Agent {version} running"));
                    if let Some(s) = &self.status {
                        ui.separator();
                        ui.label(format!("Mode: {}", mode_name(s.mode)));
                        ui.separator();
                        ui.label(format!("TV: {}", tv_name(s.tv)));
                    }
                    ui.separator();
                    match self.mode() {
                        Some(Mode::Desktop) => {
                            if ui
                                .button("Enter gaming mode")
                                .on_hover_text("Same as the gaming hotkey")
                                .clicked()
                            {
                                self.outbox.push(IpcRequest::EnterGaming);
                            }
                        }
                        Some(Mode::Gaming) => {
                            if ui.button("Exit gaming mode").clicked() {
                                self.outbox.push(IpcRequest::ExitGaming);
                            }
                        }
                        _ => {
                            ui.spinner();
                        }
                    }
                    if ui
                        .button("Panic restore")
                        .on_hover_text("Put display, Explorer and audio back, whatever the state")
                        .clicked()
                    {
                        self.outbox.push(IpcRequest::PanicRestore);
                    }
                });
                if let Some(err) = self.status.as_ref().and_then(|s| s.last_error.clone()) {
                    ui.colored_label(ui.visuals().error_fg_color, format!("Last problem: {err}"));
                }
            }
        }
        ui.add_space(4.0);
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let problems: Vec<String> = match &self.draft {
            Some(d) => {
                let mut p = self.field_errors.clone();
                match d.validate() {
                    Ok(()) => {}
                    Err(uc_core::Error::ConfigInvalid(list)) => {
                        p.extend(list.lines().map(str::to_string));
                    }
                    Err(e) => p.push(e.to_string()),
                }
                p
            }
            None => Vec::new(),
        };
        for p in &problems {
            ui.colored_label(ui.visuals().error_fg_color, p);
        }
        if let Some(e) = &self.save_error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("The agent refused to save: {e}"),
            );
        }
        ui.horizontal(|ui| {
            if self.dirty() {
                let can_save = problems.is_empty() && self.connected() && !self.busy(&save_probe());
                if ui
                    .add_enabled(can_save, egui::Button::new("Save"))
                    .clicked()
                {
                    self.save();
                }
                if ui.button("Revert").clicked() {
                    self.revert();
                }
                ui.label("Unsaved changes");
            }
            if self.busy(&save_probe()) {
                ui.spinner();
            }
            if let Some((is_error, text)) = &self.notice {
                let color = if *is_error {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().text_color()
                };
                ui.colored_label(color, text);
            }
        });
        ui.add_space(4.0);
    }
}

fn save_probe() -> IpcRequest {
    IpcRequest::SetConfig {
        config: Box::default(),
    }
}

pub fn tv_name(state: TvPowerState) -> &'static str {
    match state {
        TvPowerState::Awake => "on",
        TvPowerState::Asleep => "asleep",
        TvPowerState::Unreachable => "not reachable",
        TvPowerState::NotConfigured => "not set up",
        TvPowerState::Unknown => "unknown",
    }
}

pub fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Desktop => "Desktop",
        Mode::Entering => "Entering gaming mode…",
        Mode::Gaming => "Gaming",
        Mode::Exiting => "Leaving gaming mode…",
    }
}

/// Short name of a request for error messages.
fn what(request: &IpcRequest) -> &'static str {
    match request {
        IpcRequest::Hello { .. } => "Connect",
        IpcRequest::GetStatus => "Status",
        IpcRequest::GetConfig => "Load settings",
        IpcRequest::SetConfig { .. } => "Save",
        IpcRequest::CheckHotkey { .. } => "Shortcut check",
        IpcRequest::EnterGaming => "Enter gaming mode",
        IpcRequest::ExitGaming => "Exit gaming mode",
        IpcRequest::PanicRestore => "Panic restore",
        IpcRequest::ListDisplays => "List displays",
        IpcRequest::IdentifyDisplays => "Identify",
        IpcRequest::ListAudioDevices => "List audio devices",
        IpcRequest::TvDiscover => "Find TV",
        IpcRequest::TvPair { .. } => "Pair TV",
        IpcRequest::TvCommand { .. } => "TV test",
        IpcRequest::CheckForUpdates => "Check for updates",
        IpcRequest::SetUpdateToken { .. } => "Save token",
        IpcRequest::ClearUpdateToken => "Remove token",
        IpcRequest::Subscribe => "Subscribe",
        IpcRequest::Shutdown => "Stop agent",
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for msg in self.client.poll() {
            self.handle(msg);
        }
        // Leaving the page stops recording, so keys typed elsewhere aren't eaten.
        if self.page != Page::Hotkeys {
            self.recording = None;
        }
        self.record_hotkey(ctx);
        // "Saved." etc. is stale once the user edits again.
        if self.dirty() && matches!(self.notice, Some((false, _))) {
            self.notice = None;
        }

        egui::TopBottomPanel::top("status").show(ctx, |ui| self.header(ui));
        egui::TopBottomPanel::bottom("save").show(ctx, |ui| self.footer(ui));
        egui::SidePanel::left("pages")
            .resizable(false)
            .exact_width(150.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                for page in Page::ALL {
                    ui.selectable_value(&mut self.page, page, page.title());
                }
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| self.page_ui(ui));
        });
        self.uninstall_dialog(ctx);

        if let Some(d) = &mut self.draft {
            self.field_errors = self.fields.apply(d);
        }
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_roundtrip_through_config() {
        let mut c = Config::default();
        c.tv.ip = Some([192, 168, 1, 50].into());
        c.tv.mac = Some("A0:B1:C2:D3:E4:F5".into());
        c.tv.after_wake_commands = vec!["am start -a android.intent.action.MAIN".into()];
        c.steam.exe_path = Some(PathBuf::from(r"C:\Program Files (x86)\Steam\steam.exe"));
        let fields = Fields::from_config(&c);
        let mut back = Config::default();
        assert!(fields.apply(&mut back).is_empty());
        assert_eq!(back, c);
    }

    #[test]
    fn bad_fields_are_reported_and_keep_old_values() {
        let mut c = Config::default();
        c.tv.ip = Some([10, 0, 0, 2].into());
        let fields = Fields {
            tv_ip: "10.0.0.300".into(),
            tv_mac: "nope".into(),
            wol_address: "broadcast".into(),
            after_wake: "  one \n\n two ".into(),
            steam_exe: "\"C:\\Steam\\steam.exe\"".into(),
            launch_args: String::new(),
        };
        let errors = fields.apply(&mut c);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert_eq!(c.tv.ip, Some([10, 0, 0, 2].into()));
        assert_eq!(c.tv.after_wake_commands, ["one", "two"]);
        assert_eq!(c.steam.exe_path, Some(PathBuf::from(r"C:\Steam\steam.exe")));
        assert!(c.steam.launch_args.is_empty());
    }
}
