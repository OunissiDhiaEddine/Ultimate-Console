//! The settings pages (plan §2.10). Each edits `App::draft`; nothing reaches
//! the agent until Save. Test buttons act on the *saved* settings.

use std::collections::HashSet;
use std::mem::Discriminant;

use eframe::egui::{self, Color32, RichText, Ui};
use uc_core::config::{DisplayId, DisplayMode, Hotkeys};
use uc_core::hotkey;
use uc_core::ipc::{IpcRequest, TvCommand, TvPowerState, UpdateStatus};
use uc_core::{Config, Mode};

use crate::app::{App, HOTKEY_ACTIONS, Page, hotkey_mut, hotkey_of};
use crate::system;

fn is_busy(pending: &HashSet<Discriminant<IpcRequest>>, request: &IpcRequest) -> bool {
    pending.contains(&std::mem::discriminant(request))
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).weak());
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).strong());
    ui.add_space(2.0);
}

fn secs(ui: &mut Ui, label: &str, v: &mut u32, max: u32) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(v).range(1..=max).suffix(" s"));
    });
}

fn millis(ui: &mut Ui, label: &str, v: &mut u32, max: u32) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(
            egui::DragValue::new(v)
                .range(1..=max)
                .speed(10.0)
                .suffix(" ms"),
        );
    });
}

pub fn same_display(a: &DisplayId, b: &DisplayId) -> bool {
    if !a.device_path.is_empty() && !b.device_path.is_empty() {
        a.device_path.eq_ignore_ascii_case(&b.device_path)
    } else {
        a.friendly_name == b.friendly_name && a.connector == b.connector
    }
}

pub fn mode_label(m: &DisplayMode) -> String {
    match m.refresh_hz {
        Some(hz) if (hz - hz.round()).abs() < 0.005 => {
            format!("{}×{} @ {} Hz", m.width, m.height, hz.round())
        }
        Some(hz) => format!("{}×{} @ {hz:.2} Hz", m.width, m.height),
        None => format!("{}×{}", m.width, m.height),
    }
}

fn display_name(id: &DisplayId) -> String {
    match (id.friendly_name.as_str(), id.connector.as_str()) {
        ("", "") => "Unnamed display".into(),
        (name, "") => name.into(),
        ("", conn) => format!("Unnamed display ({conn})"),
        (name, conn) => format!("{name} ({conn})"),
    }
}

impl App {
    pub fn page_ui(&mut self, ui: &mut Ui) {
        ui.heading(self.page.title());
        ui.add_space(6.0);
        if self.page == Page::About {
            return self.about(ui);
        }
        if self.draft.is_none() {
            ui.label(if self.connected() {
                "Loading settings from the agent…"
            } else {
                "Settings are kept by the agent. Start it (button at the top) to change them."
            });
            return;
        }
        match self.page {
            Page::General => self.general(ui),
            Page::Hotkeys => self.hotkeys(ui),
            Page::Display => self.display(ui),
            Page::Audio => self.audio(ui),
            Page::Tv => self.tv(ui),
            Page::Steam => self.steam(ui),
            Page::Power => self.power(ui),
            Page::About => unreachable!(),
        }
    }

    fn draft(&mut self) -> &mut Config {
        self.draft.as_mut().expect("page_ui checked")
    }

    fn general(&mut self, ui: &mut Ui) {
        let d = self.draft();
        ui.checkbox(
            &mut d.general.start_with_windows,
            "Start the agent with Windows",
        );
        hint(
            ui,
            "Without this the hotkeys only work after starting the agent from here.",
        );
        ui.add_space(6.0);
        ui.checkbox(
            &mut d.overlay.in_desktop_mode,
            "Allow the overlay on the normal desktop too",
        );
        hint(
            ui,
            "By default the overlay hotkey only works in gaming mode.",
        );
        millis(
            ui,
            "Overlay resource refresh",
            &mut d.overlay.sample_interval_ms,
            10_000,
        );

        section(ui, "Logging");
        ui.horizontal(|ui| {
            ui.label("Log level");
            egui::ComboBox::from_id_salt("log_level")
                .selected_text(d.general.log_level.clone())
                .show_ui(ui, |ui| {
                    for level in ["error", "warn", "info", "debug", "trace"] {
                        ui.selectable_value(&mut d.general.log_level, level.to_string(), level);
                    }
                });
        });
        hint(
            ui,
            "Use debug when something misbehaves and attach the log when reporting it.",
        );
    }

    fn hotkeys(&mut self, ui: &mut Ui) {
        hint(
            ui,
            "Press Record, then the new shortcut. Esc cancels. Win+ shortcuts aren't allowed: \
             Windows reserves them and they act differently without Explorer.",
        );
        hint(
            ui,
            "The current shortcuts stay active while recording, so pressing one of them runs it.",
        );
        ui.add_space(8.0);

        let recording = self.recording;
        let d = self.draft.as_mut().expect("page_ui checked");
        let named: Vec<_> = HOTKEY_ACTIONS
            .iter()
            .map(|(action, name, _)| (*name, hotkey_of(&d.hotkeys, *action)))
            .collect();
        let local = hotkey::check_set(&named);
        egui::Grid::new("hotkeys")
            .num_columns(4)
            .spacing([12.0, 10.0])
            .show(ui, |ui| {
                for (action, name, label) in HOTKEY_ACTIONS {
                    let current = hotkey_of(&d.hotkeys, action);
                    ui.label(label);
                    let shown = if recording == Some(action) {
                        RichText::new("press a shortcut…").italics()
                    } else {
                        RichText::new(current.to_string()).monospace().strong()
                    };
                    ui.label(shown);
                    if recording == Some(action) {
                        if ui.button("Cancel").clicked() {
                            self.recording = None;
                        }
                    } else if ui.button("Record").clicked() {
                        self.recording = Some(action);
                    }
                    if ui.button("Reset").clicked() {
                        let default = hotkey_of(&Hotkeys::default(), action);
                        *hotkey_mut(&mut d.hotkeys, action) = default;
                        self.outbox.push(IpcRequest::CheckHotkey {
                            action,
                            hotkey: default.to_string(),
                        });
                    }
                    ui.end_row();

                    // Local rules (incl. "must differ") plus the agent's OS checks.
                    let mut errors = Vec::new();
                    let mut warnings = Vec::new();
                    if let Some((_, c)) = local.iter().find(|(n, _)| n == name) {
                        errors.extend(c.errors.iter().cloned());
                        warnings.extend(c.warnings.iter().cloned());
                    }
                    let mut altgr_unchecked = current.may_clash_with_altgr();
                    match self.hotkey_checks.get(&action) {
                        Some((checked, c)) if *checked == current.to_string() => {
                            for e in &c.errors {
                                if !errors.contains(e) {
                                    errors.push(e.clone());
                                }
                            }
                            for w in &c.warnings {
                                if !warnings.contains(w) {
                                    warnings.push(w.clone());
                                }
                            }
                            altgr_unchecked = c.needs_altgr_check;
                        }
                        _ => {}
                    }
                    if altgr_unchecked {
                        warnings.push(
                            "Ctrl+Alt is AltGr on AZERTY/Arabic layouts; the agent checks \
                             your layouts when it is running"
                                .into(),
                        );
                    }
                    ui.label("");
                    ui.vertical(|ui| {
                        for e in &errors {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                        for w in &warnings {
                            ui.colored_label(ui.visuals().warn_fg_color, w);
                        }
                        if errors.is_empty() && warnings.is_empty() {
                            ui.colored_label(Color32::from_rgb(80, 170, 90), "OK");
                        }
                    });
                    ui.end_row();
                }
            });
        section(ui, "Controller");
        hint(
            ui,
            "To open the overlay from a gamepad, map a Steam Input chord (for example \
             Guide + Y) to the overlay shortcut: Steam > Settings > Controller > \
             Guide Button Chord Layout > add a keyboard shortcut.",
        );
    }

    fn display(&mut self, ui: &mut Ui) {
        if self.displays.is_none() && !is_busy(&self.pending, &IpcRequest::ListDisplays) {
            self.outbox.push(IpcRequest::ListDisplays);
        }
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                self.outbox.push(IpcRequest::ListDisplays);
            }
            if ui
                .button("Identify")
                .on_hover_text("Show each display's number on it")
                .clicked()
            {
                self.outbox.push(IpcRequest::IdentifyDisplays);
            }
            if is_busy(&self.pending, &IpcRequest::ListDisplays) {
                ui.spinner();
            }
        });
        hint(
            ui,
            "Pick the TV. In gaming mode it becomes the only active display; the \
             normal layout comes back when you leave.",
        );
        hint(
            ui,
            "A TV that is off may not be listed: turn it on, then press Refresh.",
        );

        let displays = self.displays.clone().unwrap_or_default();
        let d = self.draft.as_mut().expect("page_ui checked");
        section(ui, "TV display");
        for info in &displays {
            let selected = d
                .display
                .target
                .as_ref()
                .is_some_and(|t| same_display(t, &info.id));
            let mut text = format!("#{}  {}", info.index, display_name(&info.id));
            if let Some(m) = &info.current_mode {
                text.push_str(&format!("  ·  {}", mode_label(m)));
            }
            if info.primary {
                text.push_str("  ·  primary");
            } else if !info.active {
                text.push_str("  ·  off");
            }
            if ui.radio(selected, text).clicked() && !selected {
                d.display.target = Some(info.id.clone());
                d.display.mode = None;
            }
        }
        if let Some(t) = &d.display.target
            && !displays.iter().any(|i| same_display(t, &i.id))
        {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "Saved TV \"{}\" isn't connected right now.",
                    display_name(t)
                ),
            );
        }
        if d.display.target.is_none() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "No TV picked yet: gaming mode won't start without one.",
            );
        }

        let target = d
            .display
            .target
            .as_ref()
            .and_then(|t| displays.iter().find(|i| same_display(t, &i.id)));
        section(ui, "Picture");
        ui.horizontal(|ui| {
            ui.label("Resolution");
            let current = d
                .display
                .mode
                .as_ref()
                .map(mode_label)
                .unwrap_or_else(|| "TV's preferred mode".into());
            egui::ComboBox::from_id_salt("display_mode")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut d.display.mode, None, "TV's preferred mode");
                    for m in target.map(|t| t.modes.as_slice()).unwrap_or_default() {
                        ui.selectable_value(&mut d.display.mode, Some(m.clone()), mode_label(m));
                    }
                });
        });
        let hdr_ok = target.is_none_or(|t| t.hdr_supported);
        ui.add_enabled(
            hdr_ok || d.display.hdr,
            egui::Checkbox::new(&mut d.display.hdr, "Turn HDR on while gaming"),
        );
        if !hdr_ok {
            hint(ui, "Windows doesn't report HDR support for this display.");
        }
        secs(
            ui,
            "Wait for the TV to appear",
            &mut d.display.wait_timeout_secs,
            120,
        );
        hint(
            ui,
            "After waking the TV, how long to wait for its HDMI input before giving up.",
        );
    }

    fn audio(&mut self, ui: &mut Ui) {
        if self.audio_devices.is_none() && !is_busy(&self.pending, &IpcRequest::ListAudioDevices) {
            self.outbox.push(IpcRequest::ListAudioDevices);
        }
        let devices = self.audio_devices.clone().unwrap_or_default();
        let d = self.draft.as_mut().expect("page_ui checked");
        ui.checkbox(
            &mut d.audio.switch_default,
            "Switch the default sound output while gaming",
        );
        hint(
            ui,
            "The previous output comes back when you leave gaming mode.",
        );
        ui.add_enabled_ui(d.audio.switch_default, |ui| {
            ui.horizontal(|ui| {
                ui.label("Output");
                let current = match &d.audio.device_id {
                    None => "Pick a device".to_string(),
                    Some(id) => devices
                        .iter()
                        .find(|x| &x.id == id)
                        .map(|x| x.name.clone())
                        .unwrap_or_else(|| "Saved device (not connected)".into()),
                };
                egui::ComboBox::from_id_salt("audio_device")
                    .selected_text(current)
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        for dev in &devices {
                            let mut label = dev.name.clone();
                            if dev.is_default {
                                label.push_str("  (current default)");
                            }
                            ui.selectable_value(
                                &mut d.audio.device_id,
                                Some(dev.id.clone()),
                                label,
                            );
                        }
                    });
                if ui.button("Refresh").clicked() {
                    self.outbox.push(IpcRequest::ListAudioDevices);
                }
            });
        });
        if d.audio.switch_default && d.audio.device_id.is_none() {
            ui.colored_label(ui.visuals().warn_fg_color, "Pick the TV's audio output.");
        }
    }

    fn tv(&mut self, ui: &mut Ui) {
        let dirty = self.dirty();
        let pairing = is_busy(&self.pending, &IpcRequest::TvPair { ip: [0; 4].into() });
        let discovering = is_busy(&self.pending, &IpcRequest::TvDiscover);
        let testing = is_busy(
            &self.pending,
            &IpcRequest::TvCommand {
                command: TvCommand::State,
            },
        );
        let connected = self.connected();
        let d = self.draft.as_mut().expect("page_ui checked");
        ui.checkbox(
            &mut d.tv.enabled,
            "Control the TV (wake on enter, sleep on exit)",
        );
        hint(
            ui,
            "Works with Android TVs over the network (ADB), plus Wake-on-LAN for TVs \
             that switch their network off in standby.",
        );

        egui::CollapsingHeader::new(RichText::new("Pairing wizard").strong())
            .default_open(d.tv.mac.is_none())
            .show(ui, |ui| {
                ui.label("1. On the TV:");
                hint(
                    ui,
                    "Settings > Device Preferences > About > click Build 7 times to unlock \
                     Developer options. Then Developer options > turn on Network debugging \
                     (or USB debugging + ADB over network).",
                );
                hint(
                    ui,
                    "If there's a Network standby / Wake on LAN / Wake on Wi-Fi setting, turn \
                     it on so the TV can be woken from standby.",
                );
                ui.add_space(4.0);
                ui.label("2. Find the TV, or type its IP address below:");
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(connected && !discovering, egui::Button::new("Find TV"))
                        .clicked()
                    {
                        self.outbox.push(IpcRequest::TvDiscover);
                    }
                    if discovering {
                        ui.spinner();
                        ui.label("Searching the network…");
                    }
                });
                for dev in self.tv_devices.iter().flatten() {
                    let text = format!("{}  ({}:{})", dev.name, dev.ip, dev.port);
                    if ui
                        .selectable_label(self.fields.tv_ip == dev.ip.to_string(), text)
                        .clicked()
                    {
                        self.fields.tv_ip = dev.ip.to_string();
                        d.tv.adb_port = dev.port;
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("TV IP address");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.fields.tv_ip)
                            .hint_text("192.168.1.50")
                            .desired_width(140.0),
                    );
                });
                ui.add_space(4.0);
                ui.label("3. Pair:");
                hint(
                    ui,
                    "Press Pair, then on the TV accept \"Allow debugging?\" with \"Always allow \
                     from this computer\" ticked. The TV's MAC address is filled in for you.",
                );
                let ip = self.fields.tv_ip.trim().parse().ok();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            connected && ip.is_some() && !pairing,
                            egui::Button::new("Pair"),
                        )
                        .clicked()
                        && let Some(ip) = ip
                    {
                        self.pair_result = None;
                        self.outbox.push(IpcRequest::TvPair { ip });
                    }
                    if pairing {
                        ui.spinner();
                        ui.label("Waiting for the TV… accept the prompt on screen.");
                    }
                });
                match &self.pair_result {
                    Some(Ok(text)) => {
                        ui.colored_label(Color32::from_rgb(80, 170, 90), text);
                    }
                    Some(Err(text)) => {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!("Pairing failed: {text}"),
                        );
                    }
                    None => {}
                }
            });

        section(ui, "Connection");
        ui.horizontal(|ui| {
            ui.label("IP address");
            ui.add(egui::TextEdit::singleline(&mut self.fields.tv_ip).desired_width(140.0));
            ui.label("ADB port");
            ui.add(egui::DragValue::new(&mut d.tv.adb_port).range(1..=65535));
        });
        millis(ui, "Connect timeout", &mut d.tv.connect_timeout_ms, 30_000);
        secs(
            ui,
            "Keep retrying a wake for",
            &mut d.tv.wake_retry_secs,
            120,
        );

        section(ui, "Wake-on-LAN");
        ui.checkbox(
            &mut d.tv.wol_enabled,
            "Send a Wake-on-LAN packet before waking",
        );
        ui.add_enabled_ui(d.tv.wol_enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label("TV MAC");
                ui.add(
                    egui::TextEdit::singleline(&mut self.fields.tv_mac)
                        .hint_text("AA:BB:CC:DD:EE:FF")
                        .desired_width(160.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Send to");
                ui.add(
                    egui::TextEdit::singleline(&mut self.fields.wol_address).desired_width(140.0),
                );
                ui.label("port");
                ui.add(egui::DragValue::new(&mut d.tv.wol_port).range(1..=65535));
            });
            hint(ui, "255.255.255.255 port 9 works on most home networks.");
        });

        section(ui, "After waking");
        hint(
            ui,
            "Optional adb shell commands, one per line, run after every wake (for example \
             to switch the TV to the PC's HDMI input; they depend on the TV model).",
        );
        ui.add(
            egui::TextEdit::multiline(&mut self.fields.after_wake)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );

        section(ui, "Test");
        if dirty {
            hint(
                ui,
                "Tests use the saved settings: Save first to test your changes.",
            );
        }
        ui.horizontal(|ui| {
            for (command, label) in [
                (TvCommand::Wake, "Wake"),
                (TvCommand::Sleep, "Sleep"),
                (TvCommand::State, "Is it on?"),
            ] {
                if ui
                    .add_enabled(connected && !testing, egui::Button::new(label))
                    .clicked()
                {
                    self.tv_test = None;
                    self.outbox.push(IpcRequest::TvCommand { command });
                }
            }
            if testing {
                ui.spinner();
            }
        });
        if let Some(t) = &self.tv_test {
            ui.label(t);
        }
        if self.status.as_ref().map(|s| s.tv) == Some(TvPowerState::Unreachable) {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "The agent can't reach the TV. Is it on the same network, with network \
                 debugging on?",
            );
        }
    }

    fn steam(&mut self, ui: &mut Ui) {
        let d = self.draft.as_mut().expect("page_ui checked");
        ui.horizontal(|ui| {
            ui.label("steam.exe");
            ui.add(
                egui::TextEdit::singleline(&mut self.fields.steam_exe)
                    .hint_text("found automatically")
                    .desired_width(380.0),
            );
        });
        hint(
            ui,
            "Leave empty to use the path Steam registered (HKCU\\Software\\Valve\\Steam).",
        );
        section(ui, "Launch arguments");
        hint(
            ui,
            "Used when Steam isn't running yet, one per line. -gamepadui opens Big Picture.",
        );
        ui.add(
            egui::TextEdit::multiline(&mut self.fields.launch_args)
                .desired_rows(2)
                .desired_width(380.0)
                .font(egui::TextStyle::Monospace),
        );
        secs(
            ui,
            "Wait for Big Picture to open",
            &mut d.steam.bp_launch_timeout_secs,
            300,
        );

        egui::CollapsingHeader::new("Advanced: Big Picture window").show(ui, |ui| {
            hint(
                ui,
                "How the agent recognises the Big Picture window. Only change this if a \
                 Steam update renames it.",
            );
            ui.horizontal(|ui| {
                ui.label("Window class");
                ui.text_edit_singleline(&mut d.steam.bp_window_class);
            });
            ui.horizontal(|ui| {
                ui.label("Window title");
                ui.text_edit_singleline(&mut d.steam.bp_window_title);
            });
            millis(
                ui,
                "Leave gaming mode after Big Picture is gone for",
                &mut d.steam.bp_close_debounce_ms,
                60_000,
            );
        });
    }

    fn power(&mut self, ui: &mut Ui) {
        let d = self.draft.as_mut().expect("page_ui checked");
        hint(
            ui,
            "These only apply in gaming mode started by the hotkey (or from this app). \
             On the normal desktop, or Big Picture opened by hand, sleep never touches the TV.",
        );
        ui.add_space(6.0);
        ui.checkbox(
            &mut d.power.tv_sleep_on_suspend,
            "Put the TV to sleep when the PC sleeps",
        );
        ui.checkbox(
            &mut d.power.tv_wake_on_resume,
            "Wake the TV when the PC wakes up",
        );
        section(ui, "Timing");
        millis(
            ui,
            "Time allowed for the TV sleep command",
            &mut d.power.suspend_budget_ms,
            5_000,
        );
        hint(
            ui,
            "Windows won't wait long before sleeping; the network goes down right after.",
        );
        secs(
            ui,
            "Wait for the network after waking",
            &mut d.power.network_wait_secs,
            60,
        );
    }

    fn about(&mut self, ui: &mut Ui) {
        ui.label(format!("Settings app {}", uc_core::VERSION));
        if let crate::app::AgentLink::Connected { version } = &self.link {
            ui.label(format!("Agent {version}"));
        }

        section(ui, "Updates");
        let connected = self.connected();
        let checking = is_busy(&self.pending, &IpcRequest::CheckForUpdates);
        if let Some(d) = self.draft.as_mut() {
            ui.checkbox(&mut d.updates.auto_check, "Check for updates every day");
            hint(
                ui,
                "Updates install the next time you leave gaming mode, never during a game.",
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(connected && !checking, egui::Button::new("Check now"))
                .clicked()
            {
                self.outbox.push(IpcRequest::CheckForUpdates);
            }
            if checking {
                ui.spinner();
            }
            match &self.update {
                Some(UpdateStatus::UpToDate { version }) => {
                    ui.label(format!("Up to date ({version})."));
                }
                Some(UpdateStatus::Available { version }) => {
                    ui.label(format!("Version {version} is available."));
                }
                Some(UpdateStatus::Failed { message }) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("Check failed: {message}"),
                    );
                }
                // From an agent older than 0.1.1, which still wanted a token.
                Some(UpdateStatus::TokenNeeded) | None => {}
            }
        });

        section(ui, "Maintenance");
        let gaming = self.mode().is_some_and(|m| m != Mode::Desktop);
        ui.horizontal(|ui| {
            let logs = self.paths.as_ref().map(|p| p.logs_dir());
            let open = ui
                .add_enabled(
                    !gaming && logs.is_some(),
                    egui::Button::new("Open logs folder"),
                )
                .on_disabled_hover_text(
                    "Not in gaming mode: opening a folder would start Explorer.",
                );
            if open.clicked()
                && let Some(dir) = logs
                && let Err(e) = system::open_folder(&dir)
            {
                self.error(format!("Could not open {}: {e}", dir.display()));
            }
            if ui
                .add_enabled(!self.mock, egui::Button::new("Uninstall…"))
                .on_disabled_hover_text("Not available with the mock agent.")
                .clicked()
            {
                self.confirm_uninstall = true;
            }
        });
        hint(
            ui,
            "Uninstalling puts the desktop back if needed, stops the agent and removes \
             everything: settings, logs, the TV key and the update token.",
        );
    }

    pub fn uninstall_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_uninstall {
            return;
        }
        egui::Window::new("Uninstall Ultimate Console?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    "This restores the desktop if gaming mode is on, stops the agent, and \
                     deletes all settings, logs, the TV pairing key and the update token.",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Uninstall").clicked() {
                        self.confirm_uninstall = false;
                        match system::run_uninstaller() {
                            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                            Err(e) => self.error(format!("Uninstall: {e}")),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_uninstall = false;
                    }
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(path: &str, name: &str, conn: &str) -> DisplayId {
        DisplayId {
            device_path: path.into(),
            friendly_name: name.into(),
            connector: conn.into(),
        }
    }

    #[test]
    fn displays_match_by_path_then_name() {
        assert!(same_display(
            &id(r"\\?\DISPLAY#IRS0055#a", "IRIS TV", "HDMI"),
            &id(r"\\?\display#irs0055#A", "Renamed", "HDMI")
        ));
        assert!(!same_display(
            &id(r"\\?\DISPLAY#IRS0055#a", "IRIS TV", "HDMI"),
            &id(r"\\?\DISPLAY#IRS0055#b", "IRIS TV", "HDMI")
        ));
        assert!(same_display(
            &id("", "IRIS TV", "HDMI"),
            &id(r"\\?\DISPLAY#IRS0055#a", "IRIS TV", "HDMI")
        ));
    }

    #[test]
    fn mode_labels() {
        let m = |hz| DisplayMode {
            width: 3840,
            height: 2160,
            refresh_hz: hz,
        };
        assert_eq!(mode_label(&m(Some(60.0))), "3840×2160 @ 60 Hz");
        assert_eq!(mode_label(&m(Some(59.94))), "3840×2160 @ 59.94 Hz");
        assert_eq!(mode_label(&m(None)), "3840×2160");
    }
}
