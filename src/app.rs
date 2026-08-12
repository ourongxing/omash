use crate::{
    api::{MihomoClient, Snapshot},
    backup,
    config::Config,
    core::{self, SupervisorState},
    profiles::Profiles,
    theme::Theme,
    ui, updater,
};
use anyhow::Result;
use clash_verge_media_unlock::UnlockItem;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{cmp::min, collections::HashSet, io, path::PathBuf, time::Instant};
use tokio::time;

pub const SETTINGS_COUNT: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Dashboard,
    Proxies,
    Profiles,
    Connections,
    Rules,
    Logs,
    Unlock,
    Settings,
    Help,
}

impl Tab {
    pub const ALL: [Self; 9] = [
        Self::Dashboard,
        Self::Proxies,
        Self::Profiles,
        Self::Connections,
        Self::Rules,
        Self::Logs,
        Self::Unlock,
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
            Self::Unlock => "Unlock",
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
    pub unlock_items: Vec<UnlockItem>,
    pub tab: Tab,
    pub group_index: usize,
    pub node_index: usize,
    pub connection_index: usize,
    pub rule_index: usize,
    pub profile_index: usize,
    pub setting_index: usize,
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
    mouse_regions: Vec<ui::HitRegion>,
    last_click: Option<(ui::HitTarget, Instant)>,
}

#[derive(Clone, Debug)]
pub enum InputMode {
    ImportProfile,
    RestoreBackup(PathBuf),
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let api = MihomoClient::new(&config.controller, config.secret.clone())?;
        let profiles = Profiles::load()?;
        Ok(Self {
            config,
            api,
            snapshot: Snapshot::default(),
            profiles,
            proxy_group_order: Config::proxy_group_order(),
            theme: Theme::load(),
            supervisor: core::supervisor_state(),
            logs: vec![],
            unlock_items: clash_verge_media_unlock::default_unlock_items(),
            tab: Tab::default(),
            group_index: 0,
            node_index: 0,
            connection_index: 0,
            rule_index: 0,
            profile_index: 0,
            setting_index: 0,
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
            mouse_regions: Vec::new(),
            last_click: None,
        })
    }

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
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
                        Some(Err(error)) => self.status = format!("input error: {error}"),
                        None => break,
                        _ => {}
                    }
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
                let Some(target) = target else { return };
                let now = Instant::now();
                let double_click = self.last_click.is_some_and(|(previous, then)| {
                    previous == target && now.duration_since(then).as_millis() <= 400
                });
                self.last_click = if double_click {
                    None
                } else {
                    Some((target, now))
                };
                self.activate_mouse_target(target, double_click).await;
            }
            _ => {}
        }
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
            KeyCode::Char('c') if self.tab == Tab::Unlock => self.check_unlock().await,
            KeyCode::Char('x') if self.tab == Tab::Connections => self.close_selected().await,
            KeyCode::Char('X') if self.tab == Tab::Connections => self.close_all().await,
            KeyCode::Char('d') if self.tab == Tab::Proxies => self.delay_selected().await,
            KeyCode::Char('p') if matches!(self.tab, Tab::Proxies | Tab::Rules) => {
                self.update_providers().await
            }
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
            KeyCode::Char('7') => Some(Tab::Unlock),
            KeyCode::Char('8') => Some(Tab::Settings),
            KeyCode::Char('9') => Some(Tab::Help),
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
                self.status = "Synced".into();
                self.clamp_selections();
            }
            Err(error) => {
                self.online = false;
                self.status = error.to_string();
            }
        }
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
            group
                .all
                .get(self.node_index)
                .map(|node| (name.clone(), node.clone()))
        });
        let Some((group, node)) = selected else {
            return;
        };
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
                let value = std::mem::take(&mut self.input_buffer);
                self.input = None;
                let result = if value.starts_with("http://") || value.starts_with("https://") {
                    self.profiles
                        .import_remote(&value, None, &self.config)
                        .await
                } else {
                    self.profiles
                        .import_local(std::path::Path::new(&value), None, &self.config)
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
            if enable {
                "Mihomo start requested"
            } else {
                "Mihomo stop requested"
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
                self.config.tun = !self.config.tun;
                restart = true;
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
                self.config.refresh_ms = if self.config.refresh_ms >= 5000 {
                    500
                } else {
                    self.config.refresh_ms + 500
                };
            }
            6 => {
                self.update_core().await;
                return;
            }
            7 => {
                self.update_geodata().await;
                return;
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

    async fn update_core(&mut self) {
        let before = self.snapshot.version.version.clone();
        self.status = format!("Updating Mihomo from {before}…");
        let outcome = match self.api.upgrade_core().await {
            Ok(()) => {
                time::sleep(std::time::Duration::from_millis(500)).await;
                match self.api.version().await {
                    Ok(version) => format!("Mihomo updated: {before} → {}", version.version),
                    Err(_) => "Mihomo updated; supervisor is restarting it".into(),
                }
            }
            Err(error) if error.to_string().contains("already using latest version") => {
                format!("Mihomo {before} is already the latest release")
            }
            Err(api_error) => match updater::update_core(&before).await {
                Ok(updater::CoreUpdate::AlreadyLatest(version)) => {
                    format!("Mihomo {version} is already latest (API updater: {api_error})")
                }
                Ok(updater::CoreUpdate::Installed(version)) => {
                    format!("Mihomo {version} installed; supervisor restart requested")
                }
                Err(error) => {
                    format!("Mihomo update failed: API: {api_error}; direct fallback: {error}")
                }
            },
        };
        self.refresh().await;
        self.status = outcome;
    }

    async fn update_geodata(&mut self) {
        self.status = "Updating GeoData through Mihomo…".into();
        match self.api.update_geo().await {
            Ok(()) => self.status = "GeoData updated".into(),
            Err(api_error) => match updater::update_geodata().await {
                Ok(()) => {
                    self.status =
                        format!("GeoData downloaded; restart requested (API fallback: {api_error})")
                }
                Err(error) => {
                    self.status =
                        format!("GeoData update failed: API: {api_error}; fallback: {error}")
                }
            },
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

    async fn check_unlock(&mut self) {
        self.status = "Checking media unlock…".into();
        match clash_verge_media_unlock::check_media_unlock().await {
            Ok(items) => {
                self.unlock_items = items;
                self.status = "Media unlock check completed".into();
            }
            Err(error) => self.status = format!("Unlock check failed: {error}"),
        }
    }

    async fn update_providers(&mut self) {
        self.status = "Updating providers…".into();
        match self.api.update_all_providers().await {
            Ok(count) => {
                self.status = format!("Updated {count} providers");
                self.refresh().await;
            }
            Err(error) => self.status = format!("Provider update failed: {error}"),
        }
    }
}
