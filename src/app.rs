use crate::{
    api::{MihomoClient, Snapshot},
    backup,
    config::Config,
    core::{self, SupervisorState},
    profiles::Profiles,
    theme::Theme,
    tun::toggle::{self, Decision, Progress, Setup},
    ui,
};
use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    cmp::min,
    collections::HashSet,
    io::{self, stdout},
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};
use tokio::time;

type Screen = Terminal<CrosstermBackend<io::Stdout>>;

pub fn setup_terminal() -> Result<Screen> {
    enter_screen()?;
    Ok(Terminal::new(CrosstermBackend::new(stdout()))?)
}

pub fn restore_terminal(terminal: &mut Screen) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn enter_screen() -> Result<()> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Ok(())
}

pub const SETTINGS_COUNT: usize = 7;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Dashboard,
    Proxies,
    Profiles,
    Connections,
    Rules,
    Logs,
    Settings,
    Help,
}

impl Tab {
    pub const ALL: [Self; 8] = [
        Self::Dashboard,
        Self::Proxies,
        Self::Profiles,
        Self::Connections,
        Self::Rules,
        Self::Logs,
        Self::Settings,
        Self::Help,
    ];
    pub const fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Proxies => "Proxies",
            Self::Profiles => "Profiles",
            Self::Connections => "Connections",
            Self::Rules => "Rules",
            Self::Logs => "Logs",
            Self::Settings => "Settings",
            Self::Help => "Help",
        }
    }
}

pub struct App {
    pub config: Config,
    pub api: MihomoClient,
    pub snapshot: Snapshot,
    pub profiles: Profiles,
    pub proxy_group_order: Vec<String>,
    pub theme: Theme,
    pub supervisor: SupervisorState,
    pub logs: Vec<String>,
    pub geoip_version: String,
    pub tab: Tab,
    pub group_index: usize,
    pub node_index: usize,
    pub connection_index: usize,
    pub rule_index: usize,
    pub profile_index: usize,
    pub setting_index: usize,
    /// The first visible row of each list, kept by `ui::draw` between frames.
    pub offsets: ui::ListOffsets,
    pub node_focus: bool,
    pub status: String,
    pub online: bool,
    pub last_refresh: Option<Instant>,
    pub last_profile_check: Option<Instant>,
    pub previous_totals: (u64, u64),
    pub speeds: (u64, u64),
    pub input: Option<InputMode>,
    pub input_buffer: String,
    pub help_open: bool,
    /// What the last Mihomo TUN action led to; kept until the next one.
    pub tun_notice: Option<String>,
    tun_request: Option<toggle::Request>,
    /// A helper setup confirmed in the dialog, run by `run` outside the TUI's screen.
    pending_setup: Option<Setup>,
    /// Tests fix the platform and the look at the helper.
    #[cfg(test)]
    tun_fixture: Option<(bool, toggle::Helper)>,
    mouse_regions: Vec<ui::HitRegion>,
    last_click: Option<(ui::HitTarget, Instant)>,
}

#[derive(Clone, Debug)]
pub enum InputMode {
    ImportProfile,
    RestoreBackup(PathBuf),
    InstallTunHelper(Setup),
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let profiles = Profiles::load()?;
        Self::from_parts(
            config,
            profiles,
            Config::proxy_group_order(),
            Theme::load(),
            core::supervisor_state(),
            installed_package_version("clash-geoip"),
        )
    }

    fn from_parts(
        config: Config,
        profiles: Profiles,
        proxy_group_order: Vec<String>,
        theme: Theme,
        supervisor: SupervisorState,
        geoip_version: String,
    ) -> Result<Self> {
        let api = MihomoClient::new(&config.controller, config.secret.clone())?;
        Ok(Self {
            config,
            api,
            snapshot: Snapshot::default(),
            profiles,
            proxy_group_order,
            theme,
            supervisor,
            logs: vec![],
            geoip_version,
            tab: Tab::default(),
            group_index: 0,
            node_index: 0,
            connection_index: 0,
            rule_index: 0,
            profile_index: 0,
            setting_index: 0,
            offsets: ui::ListOffsets::default(),
            node_focus: false,
            status: "Connecting…".into(),
            online: false,
            last_refresh: None,
            last_profile_check: None,
            previous_totals: (0, 0),
            speeds: (0, 0),
            input: None,
            input_buffer: String::new(),
            help_open: false,
            tun_notice: None,
            tun_request: None,
            pending_setup: None,
            #[cfg(test)]
            tun_fixture: None,
            mouse_regions: Vec::new(),
            last_click: None,
        })
    }

    pub async fn run(&mut self, terminal: &mut Screen) -> Result<()> {
        self.refresh().await;
        let mut events = EventStream::new();
        let mut tick = time::interval(self.config.refresh_interval());
        loop {
            let mut mouse_regions = Vec::new();
            terminal.draw(|frame| mouse_regions = ui::draw(frame, self))?;
            self.mouse_regions = mouse_regions;
            tokio::select! {
                _ = tick.tick() => self.refresh().await,
                event = events.next() => {
                    match event {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            if self.handle_key(key).await? { break; }
                        }
                        Some(Ok(Event::Mouse(mouse))) => self.handle_mouse(mouse).await,
                        Some(Ok(Event::Paste(text)))
                            if matches!(self.input, Some(InputMode::ImportProfile)) =>
                        {
                            self.input_buffer.push_str(text.trim());
                        }
                        Some(Err(error)) => self.status = format!("input error: {error}"),
                        None => break,
                        _ => {}
                    }
                }
            }
            if let Some(setup) = self.pending_setup.take() {
                // While it waits for input, crossterm's EventStream keeps a thread blocked on
                // reading the terminal, which could take keys of sudo's password prompt. Dropping
                // the stream wakes that thread and ends it.
                drop(events);
                let installed = install_outside(terminal, setup).await?;
                events = EventStream::new();
                tick.reset();
                match installed {
                    Ok(()) => self.apply_tun(true),
                    Err(error) => self.notify_tun(toggle::setup_failed(&format!("{error:#}"))),
                }
            }
        }
        Ok(())
    }

    async fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.input.is_some() {
            return;
        }
        let target = self
            .mouse_regions
            .iter()
            .find(|region| region.contains(mouse.column, mouse.row))
            .map(|region| region.target);
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -1
                } else {
                    1
                };
                if let Some(target) = target {
                    self.focus_mouse_target(target);
                }
                self.move_selection(delta);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((target, double_click)) = self.click(mouse.column, mouse.row) else {
                    return;
                };
                self.activate_mouse_target(target, double_click).await;
            }
            _ => {}
        }
    }

    /// The target under a left click, and whether the click completes a double-click on it.
    fn click(&mut self, column: u16, row: u16) -> Option<(ui::HitTarget, bool)> {
        let target = self
            .mouse_regions
            .iter()
            .find(|region| region.contains(column, row))
            .map(|region| region.target)?;
        let now = Instant::now();
        let double_click = self.last_click.is_some_and(|(previous, then)| {
            previous == target && now.duration_since(then).as_millis() <= 400
        });
        self.last_click = if double_click {
            None
        } else {
            Some((target, now))
        };
        Some((target, double_click))
    }

    fn focus_mouse_target(&mut self, target: ui::HitTarget) {
        match target {
            ui::HitTarget::ProxyGroup(index) => {
                self.node_focus = false;
                self.group_index = index;
            }
            ui::HitTarget::ProxyNode(index) => {
                self.node_focus = true;
                self.node_index = index;
            }
            ui::HitTarget::Profile(index) => self.profile_index = index,
            ui::HitTarget::Connection(index) => self.connection_index = index,
            ui::HitTarget::Rule(index) => self.rule_index = index,
            ui::HitTarget::Setting(index) => self.setting_index = index,
            _ => {}
        }
    }

    async fn activate_mouse_target(&mut self, target: ui::HitTarget, double_click: bool) {
        self.focus_mouse_target(target);
        match target {
            ui::HitTarget::Tab(tab) => self.tab = tab,
            ui::HitTarget::CoreToggle => self.toggle_core().await,
            ui::HitTarget::RoutingMode(mode) => self.set_mode(mode).await,
            ui::HitTarget::ProxyGroup(_) => self.node_index = 0,
            ui::HitTarget::ProxyNode(_) if double_click => self.select_node().await,
            ui::HitTarget::Profile(_) if double_click => self.select_profile().await,
            ui::HitTarget::Setting(_) if double_click => self.toggle_setting().await,
            _ => {}
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.input.is_some() {
            self.handle_input(key).await;
            return Ok(false);
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }
        if self.help_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => self.help_open = false,
                KeyCode::Char('q') => return Ok(true),
                _ => {}
            }
            return Ok(false);
        }
        if let Some(tab) = Self::tab_shortcut(&key.code) {
            self.open_tab(tab);
            return Ok(false);
        }
        if key.code == KeyCode::Char('?') {
            self.help_open = true;
            return Ok(false);
        }
        if key.code == KeyCode::Char('q') {
            return Ok(true);
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab if self.tab == Tab::Proxies => {
                self.node_focus = !self.node_focus
            }
            KeyCode::Left | KeyCode::Char('h') if self.tab == Tab::Proxies => {
                self.node_focus = false
            }
            KeyCode::Right | KeyCode::Char('l') if self.tab == Tab::Proxies => {
                self.node_focus = true
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('r') => self.refresh().await,
            KeyCode::Char('s') if self.tab == Tab::Dashboard => self.toggle_core().await,
            KeyCode::Char('m') => self.cycle_mode().await,
            KeyCode::Char('a') if self.tab == Tab::Profiles => {
                self.input = Some(InputMode::ImportProfile);
                self.input_buffer.clear();
            }
            KeyCode::Char('u') if self.tab == Tab::Profiles => self.update_profile().await,
            KeyCode::Char('D') if self.tab == Tab::Profiles => self.delete_profile().await,
            KeyCode::Char('x') if self.tab == Tab::Connections => self.close_selected().await,
            KeyCode::Char('X') if self.tab == Tab::Connections => self.close_all().await,
            KeyCode::Char('d') if self.tab == Tab::Proxies => self.delay_selected().await,
            KeyCode::Enter if self.tab == Tab::Proxies => self.select_node().await,
            KeyCode::Enter if self.tab == Tab::Profiles => self.select_profile().await,
            KeyCode::Enter if self.tab == Tab::Settings => self.toggle_setting().await,
            KeyCode::Char('b') if self.tab == Tab::Settings => self.create_backup(),
            KeyCode::Char('R') if self.tab == Tab::Settings => self.confirm_restore_backup(),
            _ => {}
        }
        Ok(false)
    }

    pub fn proxy_groups(&self) -> Vec<(&String, &crate::api::Proxy)> {
        let mut values: Vec<_> = self
            .proxy_group_order
            .iter()
            .filter_map(|name| self.snapshot.proxies.proxies.get_key_value(name))
            .filter(|(_, proxy)| !proxy.all.is_empty())
            .collect();
        let configured: HashSet<_> = self.proxy_group_order.iter().collect();
        let mut unconfigured: Vec<_> = self
            .snapshot
            .proxies
            .proxies
            .iter()
            .filter(|(name, proxy)| !proxy.all.is_empty() && !configured.contains(name))
            .collect();
        unconfigured.sort_by_key(|item| item.0.to_lowercase());
        values.extend(unconfigured);
        values
    }

    pub fn selected_group(&self) -> Option<(&String, &crate::api::Proxy)> {
        self.proxy_groups().get(self.group_index).copied()
    }

    pub fn selected_group_is_manual(&self) -> bool {
        self.selected_group()
            .is_some_and(|(_, group)| group.kind.eq_ignore_ascii_case("selector"))
    }

    fn open_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    fn tab_shortcut(code: &KeyCode) -> Option<Tab> {
        match code {
            KeyCode::Char('1') => Some(Tab::Dashboard),
            KeyCode::Char('2') => Some(Tab::Proxies),
            KeyCode::Char('3') => Some(Tab::Profiles),
            KeyCode::Char('4') => Some(Tab::Connections),
            KeyCode::Char('5') => Some(Tab::Rules),
            KeyCode::Char('6') => Some(Tab::Logs),
            KeyCode::Char('7') => Some(Tab::Settings),
            KeyCode::Char('8') => Some(Tab::Help),
            _ => None,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let group_len = self.proxy_groups().len();
        let (index, len) = match self.tab {
            Tab::Proxies if self.node_focus => {
                let len = self
                    .selected_group()
                    .map_or(0, |(_, proxy)| proxy.all.len());
                (&mut self.node_index, len)
            }
            Tab::Proxies => (&mut self.group_index, group_len),
            Tab::Profiles => (&mut self.profile_index, self.profiles.items.len()),
            Tab::Connections => (
                &mut self.connection_index,
                self.snapshot.connections.connections.len(),
            ),
            Tab::Rules => (&mut self.rule_index, self.snapshot.rules.rules.len()),
            Tab::Settings => (&mut self.setting_index, SETTINGS_COUNT),
            _ => return,
        };
        if len == 0 {
            *index = 0;
            return;
        }
        *index = ((*index as isize + delta).rem_euclid(len as isize)) as usize;
        if self.tab == Tab::Proxies && !self.node_focus {
            self.node_index = 0;
        }
    }

    async fn refresh(&mut self) {
        self.theme.refresh();
        self.proxy_group_order = Config::proxy_group_order();
        self.update_due_profiles().await;
        self.supervisor = core::supervisor_state();
        if let Some(tun_enabled) = Config::saved_tun_enabled() {
            self.config.tun_enabled = tun_enabled;
        }
        self.logs = core::CoreManager::recent_logs(200).unwrap_or_default();
        match self.api.snapshot().await {
            Ok(snapshot) => {
                let totals = (
                    snapshot.connections.upload_total,
                    snapshot.connections.download_total,
                );
                let elapsed = self
                    .last_refresh
                    .map_or(1.0, |then| then.elapsed().as_secs_f64())
                    .max(0.1);
                self.speeds = if self.last_refresh.is_some() {
                    (
                        (totals.0.saturating_sub(self.previous_totals.0) as f64 / elapsed) as u64,
                        (totals.1.saturating_sub(self.previous_totals.1) as f64 / elapsed) as u64,
                    )
                } else {
                    (0, 0)
                };
                self.previous_totals = totals;
                self.last_refresh = Some(Instant::now());
                self.snapshot = snapshot;
                self.online = true;
                self.status = self
                    .supervisor
                    .error
                    .clone()
                    .unwrap_or_else(|| "Synced".into());
                self.clamp_selections();
            }
            Err(error) => {
                self.online = false;
                self.status = self.offline_status(&error.to_string());
            }
        }
        self.follow_tun_request();
    }

    fn offline_status(&self, api_error: &str) -> String {
        if self.profiles.items.is_empty() {
            return "Proxy core is not running: no profile imported. Open Profiles and press a to import."
                .into();
        }
        if !core::core_desired_enabled() {
            return "Proxy core is stopped: disabled in Settings.".into();
        }
        self.supervisor
            .error
            .as_ref()
            .map(|error| format!("Proxy core is not running: {error}"))
            .unwrap_or_else(|| format!("Core API unavailable: {api_error}"))
    }

    async fn update_due_profiles(&mut self) {
        if self
            .last_profile_check
            .is_some_and(|last| last.elapsed().as_secs() < 60)
        {
            return;
        }
        self.last_profile_check = Some(Instant::now());
        let now = chrono::Utc::now().timestamp();
        let due: Vec<_> = self
            .profiles
            .items
            .iter()
            .filter(|profile| {
                profile.url.is_some()
                    && profile.update_interval.is_some_and(|hours| {
                        now.saturating_sub(profile.updated) >= (hours.saturating_mul(3600)) as i64
                    })
            })
            .map(|profile| profile.uid.clone())
            .collect();
        let current = self.profiles.current.clone();
        let mut reload = false;
        for uid in due {
            match self.profiles.update_validated(&uid, &self.config).await {
                Ok(()) => reload |= current.as_deref() == Some(&uid),
                Err(error) => {
                    self.status = format!("Auto-update {uid} failed: {error}");
                    return;
                }
            }
        }
        if reload && let Err(error) = core::request_restart() {
            self.status = format!("Auto-update applied, restart request failed: {error}");
        }
    }

    fn clamp_selections(&mut self) {
        self.group_index = min(
            self.group_index,
            self.proxy_groups().len().saturating_sub(1),
        );
        let node_len = self.selected_group().map_or(0, |(_, p)| p.all.len());
        self.node_index = min(self.node_index, node_len.saturating_sub(1));
        self.connection_index = min(
            self.connection_index,
            self.snapshot
                .connections
                .connections
                .len()
                .saturating_sub(1),
        );
        self.rule_index = min(
            self.rule_index,
            self.snapshot.rules.rules.len().saturating_sub(1),
        );
        self.profile_index = min(
            self.profile_index,
            self.profiles.items.len().saturating_sub(1),
        );
    }

    async fn cycle_mode(&mut self) {
        let mode = match self.snapshot.config.mode.to_ascii_lowercase().as_str() {
            "rule" => "global",
            "global" => "direct",
            _ => "rule",
        };
        self.set_mode(mode).await;
    }

    async fn set_mode(&mut self, mode: &str) {
        if self.snapshot.config.mode.eq_ignore_ascii_case(mode) {
            return;
        }
        match self.api.set_mode(mode).await {
            Ok(()) => {
                self.status = format!("Mode changed to {mode}");
                self.refresh().await;
            }
            Err(error) => self.status = format!("Mode change failed: {error}"),
        }
    }

    async fn select_node(&mut self) {
        let selected = self.selected_group().and_then(|(name, group)| {
            group.all.get(self.node_index).map(|node| {
                (
                    name.clone(),
                    node.clone(),
                    group.kind.eq_ignore_ascii_case("selector"),
                )
            })
        });
        let Some((group, node, manual)) = selected else {
            return;
        };
        if !manual {
            self.status = format!("{group} is managed automatically");
            return;
        }
        match self.api.select_proxy(&group, &node).await {
            Ok(()) => {
                self.status = match self.profiles.record_selection(&group, &node) {
                    Ok(()) => format!("{group} → {node}"),
                    Err(error) => format!("{group} → {node}; selection was not saved: {error}"),
                };
                self.refresh().await;
            }
            Err(error) => self.status = format!("Selection failed: {error}"),
        }
    }

    async fn delay_selected(&mut self) {
        let node = self
            .selected_group()
            .and_then(|(_, g)| g.all.get(self.node_index))
            .cloned();
        let Some(node) = node else { return };
        self.status = format!("Testing {node}…");
        match self
            .api
            .test_delay(&node, &self.config.delay_test_url)
            .await
        {
            Ok(delay) => self.status = format!("{node}: {delay} ms"),
            Err(error) => self.status = format!("Delay test failed: {error}"),
        }
    }

    async fn close_selected(&mut self) {
        let id = self
            .snapshot
            .connections
            .connections
            .get(self.connection_index)
            .map(|c| c.id.clone());
        let Some(id) = id else { return };
        match self.api.close_connection(Some(&id)).await {
            Ok(()) => {
                self.status = "Connection closed".into();
                self.refresh().await;
            }
            Err(error) => self.status = format!("Close failed: {error}"),
        }
    }

    async fn close_all(&mut self) {
        match self.api.close_connection(None).await {
            Ok(()) => {
                self.status = "All connections closed".into();
                self.refresh().await;
            }
            Err(error) => self.status = format!("Close failed: {error}"),
        }
    }

    async fn handle_input(&mut self, key: KeyEvent) {
        if let Some(InputMode::InstallTunHelper(setup)) = self.input.clone() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.input = None;
                    self.pending_setup = Some(setup);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.input = None;
                    self.notify_tun(toggle::declined(setup).into());
                }
                _ => {}
            }
            return;
        }
        if let Some(InputMode::RestoreBackup(path)) = self.input.clone() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.input = None;
                    match backup::restore(&path) {
                        Ok(()) => match Profiles::load() {
                            Ok(profiles) => {
                                self.profiles = profiles;
                                self.status = format!("Restored {}", path.display());
                            }
                            Err(error) => {
                                self.status = format!("Restored, but reload failed: {error}")
                            }
                        },
                        Err(error) => self.status = format!("Restore failed: {error}"),
                    }
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.input = None;
                    self.status = "Restore cancelled".into();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                self.input_buffer.clear();
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(character) => self.input_buffer.push(character),
            KeyCode::Enter => {
                let value = self.input_buffer.trim().to_owned();
                if value.is_empty() {
                    self.status =
                        "Enter a subscription URL or an absolute YAML/JSON file path.".into();
                    return;
                }
                self.input_buffer.clear();
                self.input = None;
                let value = value.strip_prefix("file://").unwrap_or(&value);
                let result = if value.starts_with("http://") || value.starts_with("https://") {
                    self.profiles.import_remote(value, None, &self.config).await
                } else {
                    self.profiles
                        .import_local(std::path::Path::new(value), None, &self.config)
                        .await
                };
                match result {
                    Ok(uid) => {
                        self.status = format!("Imported {uid}");
                        self.profile_index = self.profiles.items.len().saturating_sub(1);
                    }
                    Err(error) => self.status = format!("Import failed: {error}"),
                }
            }
            _ => {}
        }
    }

    async fn toggle_core(&mut self) {
        let enable = !core::core_desired_enabled();
        let result = core::request_core_enabled(enable).map(|()| {
            if enable && self.profiles.items.is_empty() {
                "The core cannot start: no profile imported. Open Profiles and press a to import."
            } else if enable {
                "Core start requested"
            } else {
                "Core stop requested"
            }
            .to_owned()
        });
        match result {
            Ok(message) => self.status = message,
            Err(error) => self.status = format!("Core operation failed: {error}"),
        }
        self.refresh().await;
    }

    async fn select_profile(&mut self) {
        let Some(uid) = self
            .profiles
            .items
            .get(self.profile_index)
            .map(|item| item.uid.clone())
        else {
            return;
        };
        self.status = format!("Validating profile {uid}…");
        let mut candidate = self.profiles.clone();
        candidate.current = Some(uid.clone());
        match core::CoreManager::new()
            .validate_only(&self.config, &candidate)
            .await
        {
            Ok(()) => match candidate.save().and_then(|_| core::request_restart()) {
                Ok(()) => {
                    self.profiles = candidate;
                    self.status = format!("Profile {uid} activated");
                }
                Err(error) => {
                    self.status = format!("Profile was valid but could not be activated: {error}")
                }
            },
            Err(error) => self.status = format!("Profile rejected; current core kept: {error}"),
        }
        self.refresh().await;
    }

    async fn update_profile(&mut self) {
        let Some(uid) = self
            .profiles
            .items
            .get(self.profile_index)
            .map(|item| item.uid.clone())
        else {
            return;
        };
        match self.profiles.update_validated(&uid, &self.config).await {
            Ok(()) => self.status = format!("Profile {uid} updated"),
            Err(error) => self.status = format!("Update failed: {error}"),
        }
    }

    async fn delete_profile(&mut self) {
        let Some(uid) = self
            .profiles
            .items
            .get(self.profile_index)
            .map(|item| item.uid.clone())
        else {
            return;
        };
        match self.profiles.delete(&uid) {
            Ok(()) => self.status = format!("Profile {uid} deleted"),
            Err(error) => self.status = format!("Delete failed: {error}"),
        }
    }

    async fn toggle_setting(&mut self) {
        let mut restart = false;
        match self.setting_index {
            0 => {
                let enable = !core::core_desired_enabled();
                if let Err(error) = core::request_core_enabled(enable) {
                    self.status = format!("Core state change failed: {error}");
                    return;
                }
            }
            1 => {
                let previous = self.config.auto_start;
                self.config.auto_start = !previous;
                if let Err(error) = self.config.save() {
                    self.config.auto_start = previous;
                    self.status = format!("Save failed: {error}");
                    return;
                }
                if let Err(error) = core::set_supervisor_autostart(self.config.auto_start).await {
                    self.config.auto_start = previous;
                    let rollback = self.config.save().err();
                    self.status = format!("Autostart change failed: {error}");
                    if let Some(rollback) = rollback {
                        self.status
                            .push_str(&format!("; rollback failed: {rollback}"));
                    }
                    return;
                }
                self.status = "Autostart setting saved".into();
                return;
            }
            2 => {
                self.config.system_proxy = !self.config.system_proxy;
                restart = true;
            }
            3 => {
                self.config.allow_lan = !self.config.allow_lan;
                restart = true;
            }
            4 => {
                self.config.ipv6 = !self.config.ipv6;
                restart = true;
            }
            5 => {
                self.toggle_tun().await;
                return;
            }
            6 => {
                self.config.refresh_ms = if self.config.refresh_ms >= 5000 {
                    500
                } else {
                    self.config.refresh_ms + 500
                };
            }
            _ => {}
        }
        if let Err(error) = self.config.save() {
            self.status = format!("Save failed: {error}");
            return;
        }
        if restart && let Err(error) = core::request_restart() {
            self.status = format!("Saved, restart request failed: {error}");
            return;
        }
        self.status = "Setting saved".into();
    }

    /// The Mihomo TUN switch, with the rules of `omash tun on/off`. Turning TUN on first offers to
    /// install a missing or outdated helper; declining that leaves TUN off.
    async fn toggle_tun(&mut self) {
        self.tun_request = None;
        self.tun_notice = None;
        #[cfg(test)]
        let fixture = self.tun_fixture;
        #[cfg(not(test))]
        let fixture: Option<(bool, toggle::Helper)> = None;
        let enable = !self.config.tun_enabled;
        let linux = fixture.map_or(cfg!(target_os = "linux"), |(linux, _)| linux);
        let look = || async move {
            match fixture {
                Some((_, helper)) => helper,
                None => look_at_helper().await,
            }
        };
        match toggle::decide(linux, self.profiles.current_core(), enable, look).await {
            Decision::Refuse(reason) => self.notify_tun(reason.into()),
            Decision::Offer(setup) => self.input = Some(InputMode::InstallTunHelper(setup)),
            Decision::Apply => self.apply_tun(enable),
        }
    }

    fn apply_tun(&mut self, enable: bool) {
        let previous = self.config.tun_enabled;
        self.config.tun_enabled = enable;
        if let Err(error) = self.config.save() {
            self.config.tun_enabled = previous;
            self.notify_tun(format!("Save failed: {error}"));
            return;
        }
        // Turning TUN on also starts a core that was stopped, as `omash tun on` does.
        let requested = if enable {
            core::request_core_enabled(true)
        } else {
            core::request_restart()
        };
        if let Err(error) = requested {
            self.notify_tun(format!("Saved, restart request failed: {error}"));
            return;
        }
        self.tun_request = Some(toggle::Request::new(core::desired_revision(), enable));
        self.notify_tun(toggle::APPLYING.into());
    }

    fn notify_tun(&mut self, message: String) {
        self.status = message.clone();
        self.tun_notice = Some(message);
    }

    /// Replaces "Applying TUN…" with the result once the supervisor has one. `refresh` has read
    /// `tun_enabled` from config.toml already.
    fn follow_tun_request(&mut self) {
        let Some(request) = &mut self.tun_request else {
            return;
        };
        if !request.follow(&core::desired_revision(), self.config.tun_enabled) {
            self.tun_request = None;
            self.notify_tun(toggle::SUPERSEDED.into());
            return;
        }
        match request.progress(&self.supervisor) {
            Progress::Pending => {}
            Progress::Done(message) | Progress::Failed(message) => {
                self.tun_request = None;
                self.notify_tun(message);
            }
        }
    }

    fn create_backup(&mut self) {
        match backup::create() {
            Ok(path) => self.status = format!("Backup created: {}", path.display()),
            Err(error) => self.status = format!("Backup failed: {error}"),
        }
    }

    fn confirm_restore_backup(&mut self) {
        match backup::list() {
            Ok(files) if files.is_empty() => self.status = "No local backups".into(),
            Ok(files) => self.input = Some(InputMode::RestoreBackup(files[0].clone())),
            Err(error) => self.status = format!("Cannot list backups: {error}"),
        }
    }
}

/// One look at the helper before TUN is turned on, bounded by `toggle::HELPER_LOOK`.
async fn look_at_helper() -> toggle::Helper {
    if !Path::new(crate::tun::service::SETTINGS).exists() {
        return toggle::classify(false, None);
    }
    // A second connection never owns the supervisor's lease; dropping it stops nothing.
    let error =
        match time::timeout(toggle::HELPER_LOOK, crate::tun::protocol::Client::connect()).await {
            Ok(Ok(_)) | Err(_) => None,
            Ok(Err(error)) => Some(error),
        };
    toggle::classify(true, error.as_ref())
}

/// Runs the helper setup in the normal screen, where sudo can ask for the password, and returns to
/// the TUI's screen afterwards. Only an error of the terminal itself is returned as `Err`.
async fn install_outside(terminal: &mut Screen, setup: Setup) -> Result<Result<()>> {
    restore_terminal(terminal)?;
    println!(
        "{} the Mihomo TUN helper. sudo asks for your administrator password.",
        match setup {
            Setup::Install => "Installing",
            Setup::Update => "Updating",
        }
    );
    let installed = crate::tun::install::install_helper().await;
    match &installed {
        Ok(()) => println!("TUN helper installed. Returning to omash turns TUN on."),
        Err(error) => eprintln!("TUN helper setup failed: {error:#}"),
    }
    println!("Press Enter to return to omash");
    // Read in the normal (cooked) mode; Ctrl-C here ends omash, not the background supervisor.
    let _ = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        io::stdin().read_line(&mut line)
    })
    .await;
    enter_screen()?;
    // Force a complete repaint. `Terminal::clear` would first ask the terminal for the cursor
    // position and fail where no answer comes; a fullscreen resize clears the screen and the
    // previous frame without asking.
    let area = terminal.size()?.into();
    terminal.resize(area)?;
    Ok(installed)
}

fn installed_package_version(name: &str) -> String {
    Command::new("pacman")
        .args(["-Q", name])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|line| {
            line.split_once(' ')
                .map(|(_, version)| version.trim().to_owned())
        })
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "not installed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn draw(app: &mut App, terminal: &mut Terminal<TestBackend>) {
        let mut regions = Vec::new();
        terminal
            .draw(|frame| regions = ui::draw(frame, app))
            .unwrap();
        app.mouse_regions = regions;
    }

    /// More rows than any of the tested terminals shows, so that every list scrolls.
    const ROWS: usize = 30;

    #[derive(Clone, Copy, Debug)]
    enum Rows {
        Groups,
        Nodes,
        Profiles,
        Connections,
        Rules,
    }
    const LISTS: [Rows; 5] = [
        Rows::Groups,
        Rows::Nodes,
        Rows::Profiles,
        Rows::Connections,
        Rows::Rules,
    ];

    impl Rows {
        fn label(self, index: usize) -> String {
            match self {
                Rows::Groups => format!("grp{index:02}"),
                Rows::Nodes => format!("nd{index:02}"),
                Rows::Profiles => format!("prf{index:02}"),
                Rows::Connections => format!("host{index:02}.test"),
                Rows::Rules => format!("rule{index:02}.test"),
            }
        }

        fn index(self, target: ui::HitTarget) -> Option<usize> {
            match (self, target) {
                (Rows::Groups, ui::HitTarget::ProxyGroup(index))
                | (Rows::Nodes, ui::HitTarget::ProxyNode(index))
                | (Rows::Profiles, ui::HitTarget::Profile(index))
                | (Rows::Connections, ui::HitTarget::Connection(index))
                | (Rows::Rules, ui::HitTarget::Rule(index)) => Some(index),
                _ => None,
            }
        }

        fn select(self, app: &mut App, index: usize) {
            match self {
                Rows::Groups => {
                    app.tab = Tab::Proxies;
                    app.node_focus = false;
                    app.group_index = index;
                }
                Rows::Nodes => {
                    app.tab = Tab::Proxies;
                    app.node_focus = true;
                    app.node_index = index;
                }
                Rows::Profiles => {
                    app.tab = Tab::Profiles;
                    app.profile_index = index;
                }
                Rows::Connections => {
                    app.tab = Tab::Connections;
                    app.connection_index = index;
                }
                Rows::Rules => {
                    app.tab = Tab::Rules;
                    app.rule_index = index;
                }
            }
        }
    }

    /// An App whose lists all hold `ROWS` rows, built without reading or writing any file.
    fn app_with_rows() -> App {
        let profiles = Profiles {
            items: (0..ROWS)
                .map(|index| crate::profiles::Profile {
                    uid: format!("u{index}"),
                    name: Rows::Profiles.label(index),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut app = App::from_parts(
            Config::default(),
            profiles,
            Vec::new(),
            Theme::default(),
            SupervisorState::default(),
            String::new(),
        )
        .unwrap();
        let nodes: Vec<_> = (0..ROWS).map(|index| Rows::Nodes.label(index)).collect();
        let groups: serde_json::Map<_, _> = (0..ROWS)
            .map(|index| {
                (
                    Rows::Groups.label(index),
                    serde_json::json!({"type": "Selector", "now": nodes[0], "all": nodes}),
                )
            })
            .collect();
        app.snapshot.proxies =
            serde_json::from_value(serde_json::json!({ "proxies": groups })).unwrap();
        app.snapshot.connections = serde_json::from_value(serde_json::json!({
            "connections": (0..ROWS)
                .map(|index| serde_json::json!({
                    "id": format!("c{index}"),
                    "metadata": {"host": Rows::Connections.label(index)},
                }))
                .collect::<Vec<_>>(),
        }))
        .unwrap();
        app.snapshot.rules = serde_json::from_value(serde_json::json!({
            "rules": (0..ROWS)
                .map(|index| serde_json::json!({
                    "type": "DOMAIN", "payload": Rows::Rules.label(index), "proxy": "DIRECT",
                }))
                .collect::<Vec<_>>(),
        }))
        .unwrap();
        app
    }

    fn row_text(terminal: &Terminal<TestBackend>, area: ratatui::layout::Rect) -> String {
        (area.left()..area.right())
            .map(|x| terminal.backend().buffer()[(x, area.y)].symbol().to_owned())
            .collect()
    }

    #[test]
    fn every_list_region_covers_its_rendered_row() {
        for (width, height) in [(80, 24), (140, 24), (140, 40)] {
            for list in LISTS {
                let mut app = app_with_rows();
                list.select(&mut app, ROWS - 1);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                draw(&mut app, &mut terminal);
                let regions: Vec<_> = app
                    .mouse_regions
                    .iter()
                    .filter_map(|region| {
                        list.index(region.target).map(|index| (index, region.area))
                    })
                    .collect();
                assert!(
                    regions.iter().any(|(index, _)| *index == ROWS - 1),
                    "{width}x{height} {list:?}: the selected row has no region"
                );
                for (index, area) in regions {
                    let shown = row_text(&terminal, area);
                    assert!(
                        shown.contains(&list.label(index)),
                        "{width}x{height} {list:?}: the region of row {index} covers {shown:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_double_click_stays_on_its_row_in_every_list() {
        for (width, height) in [(80, 24), (140, 24), (140, 40)] {
            for list in LISTS {
                let mut app = app_with_rows();
                list.select(&mut app, ROWS - 1);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                draw(&mut app, &mut terminal);
                // The topmost visible row: not the selected (last) one, since every list scrolls.
                let (target, area) = app
                    .mouse_regions
                    .iter()
                    .filter(|region| list.index(region.target).is_some())
                    .min_by_key(|region| region.area.y)
                    .map(|region| (region.target, region.area))
                    .unwrap_or_else(|| panic!("{width}x{height} {list:?}: no rows"));
                assert_ne!(list.index(target), Some(ROWS - 1));
                assert_eq!(app.click(area.x, area.y), Some((target, false)));
                app.focus_mouse_target(target);
                draw(&mut app, &mut terminal);
                assert_eq!(
                    app.click(area.x, area.y),
                    Some((target, true)),
                    "{width}x{height} {list:?}: the second click left the row it selected"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_double_click_on_the_tun_row_stays_on_it_when_the_list_scrolls() {
        let mut app = App::from_parts(
            Config::default(),
            Profiles::default(),
            Vec::new(),
            Theme::default(),
            SupervisorState::default(),
            String::new(),
        )
        .unwrap();
        app.tab = Tab::Settings;
        // 80x24 shows three settings rows: with Refresh interval selected, Mihomo TUN is the second.
        for (width, height) in [(80, 24), (140, 24), (140, 40)] {
            app.setting_index = SETTINGS_COUNT - 1;
            app.offsets.settings.set(0);
            app.last_click = None;
            app.input = None;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            draw(&mut app, &mut terminal);
            let row = app
                .mouse_regions
                .iter()
                .find(|region| region.target == ui::HitTarget::Setting(5))
                .unwrap_or_else(|| panic!("{width}x{height}: Mihomo TUN has no region"))
                .area;
            let shown: String = (row.left()..row.right())
                .map(|x| terminal.backend().buffer()[(x, row.y)].symbol().to_owned())
                .collect();
            assert!(shown.contains("Mihomo TUN"), "{width}x{height}: {shown:?}");
            // The first click selects the row (what `activate_mouse_target` does before acting).
            let first = app.click(row.x, row.y);
            assert_eq!(first, Some((ui::HitTarget::Setting(5), false)));
            app.focus_mouse_target(ui::HitTarget::Setting(5));
            draw(&mut app, &mut terminal);
            assert_eq!(
                app.click(row.x, row.y),
                Some((ui::HitTarget::Setting(5), true)),
                "{width}x{height}: the second click left the Mihomo TUN row"
            );
            // The same two clicks through `handle_mouse` open the installation dialog. Only now,
            // with both clicks shown to land on Mihomo TUN, is anything dispatched: a click on
            // another setting would save the real config.toml. The fixture keeps the switch
            // from looking at this machine's helper.
            app.setting_index = SETTINGS_COUNT - 1;
            app.offsets.settings.set(0);
            app.last_click = None;
            app.input = None;
            app.tun_fixture = Some((true, toggle::Helper::Missing));
            let press = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: row.x,
                row: row.y,
                modifiers: KeyModifiers::NONE,
            };
            draw(&mut app, &mut terminal);
            app.handle_mouse(press).await;
            assert!(app.input.is_none(), "{width}x{height}: one click acted");
            draw(&mut app, &mut terminal);
            app.handle_mouse(press).await;
            assert!(
                matches!(app.input, Some(InputMode::InstallTunHelper(Setup::Install))),
                "{width}x{height}: the double-click did not offer to install the helper: {:?}",
                app.input
            );
            assert!(!app.config.tun_enabled, "the dialog alone turned TUN on");
        }
    }
}
