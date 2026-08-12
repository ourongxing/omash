use crate::app::{App, Tab};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Tabs, Wrap,
    },
};

const ACCENT: Color = Color::Rgb(120, 190, 255);
const MUTED: Color = Color::Rgb(125, 135, 150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitTarget {
    Tab(Tab),
    CoreToggle,
    ModeCycle,
    ProxyGroup(usize),
    ProxyNode(usize),
    Profile(usize),
    Connection(usize),
    Rule(usize),
    Setting(usize),
}

#[derive(Clone, Copy, Debug)]
pub struct HitRegion {
    pub area: Rect,
    pub target: HitTarget,
}

impl HitRegion {
    pub fn contains(self, x: u16, y: u16) -> bool {
        x >= self.area.x && x < self.area.right() && y >= self.area.y && y < self.area.bottom()
    }
}

pub fn draw(frame: &mut Frame, app: &App) -> Vec<HitRegion> {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_header(frame, app, areas[0]);
    match app.tab {
        Tab::Dashboard => dashboard(frame, app, areas[1]),
        Tab::Proxies => proxies(frame, app, areas[1]),
        Tab::Profiles => profiles(frame, app, areas[1]),
        Tab::Connections => connections(frame, app, areas[1]),
        Tab::Rules => rules(frame, app, areas[1]),
        Tab::Logs => logs(frame, app, areas[1]),
        Tab::Unlock => unlock(frame, app, areas[1]),
        Tab::Settings => settings(frame, app, areas[1]),
        Tab::Help => help(frame, app, areas[1]),
    }
    draw_status(frame, app, areas[2]);
    if app.input.is_some() {
        draw_input(frame, app);
    }
    hit_regions(app, areas[0], areas[1])
}

fn hit_regions(app: &App, header: Rect, content: Rect) -> Vec<HitRegion> {
    let mut regions = tab_regions(header);
    match app.tab {
        Tab::Dashboard => {
            let rows = Layout::vertical([Constraint::Length(7), Constraint::Min(5)]).split(content);
            let cards = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(rows[0]);
            regions.push(HitRegion {
                area: cards[0],
                target: HitTarget::CoreToggle,
            });
            regions.push(HitRegion {
                area: cards[1],
                target: HitTarget::ModeCycle,
            });
        }
        Tab::Proxies => {
            let columns =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .split(content);
            regions.extend(list_regions(
                columns[0],
                app.proxy_groups().len(),
                app.group_index,
                false,
                HitTarget::ProxyGroup,
            ));
            regions.extend(list_regions(
                columns[1],
                app.selected_group().map_or(0, |(_, group)| group.all.len()),
                app.node_index,
                false,
                HitTarget::ProxyNode,
            ));
        }
        Tab::Profiles => regions.extend(list_regions(
            content,
            app.profiles.items.len(),
            app.profile_index,
            true,
            HitTarget::Profile,
        )),
        Tab::Connections => regions.extend(list_regions(
            content,
            app.snapshot.connections.connections.len(),
            app.connection_index,
            true,
            HitTarget::Connection,
        )),
        Tab::Rules => regions.extend(list_regions(
            content,
            app.snapshot.rules.rules.len(),
            app.rule_index,
            true,
            HitTarget::Rule,
        )),
        Tab::Settings => regions.extend(list_regions(
            content,
            crate::app::SETTINGS_COUNT,
            app.setting_index,
            false,
            HitTarget::Setting,
        )),
        _ => {}
    }
    regions
}

fn tab_regions(area: Rect) -> Vec<HitRegion> {
    let mut x = area.x.saturating_add(1);
    Tab::ALL
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(index, tab)| {
            let width = format!(" {} {} ", index + 1, tab.title()).chars().count() as u16;
            let available = area.right().saturating_sub(x);
            let visible_width = width.min(available);
            let region = (visible_width > 0).then_some(HitRegion {
                area: Rect::new(x, area.y.saturating_add(1), visible_width, 1),
                target: HitTarget::Tab(tab),
            });
            x = x.saturating_add(width).saturating_add(1);
            region
        })
        .collect()
}

fn list_regions(
    area: Rect,
    len: usize,
    selected: usize,
    has_header: bool,
    target: fn(usize) -> HitTarget,
) -> Vec<HitRegion> {
    let inner = area.inner(Margin::new(1, 1));
    let header_height = u16::from(has_header);
    let capacity = inner.height.saturating_sub(header_height) as usize;
    if capacity == 0 || len == 0 {
        return Vec::new();
    }
    let start = visible_start(selected, len, capacity);
    let visible = (len - start).min(capacity);
    (0..visible)
        .map(|offset| HitRegion {
            area: Rect::new(
                inner.x,
                inner.y + header_height + offset as u16,
                inner.width,
                1,
            ),
            target: target(start + offset),
        })
        .collect()
}

fn visible_start(selected: usize, len: usize, capacity: usize) -> usize {
    if len <= capacity || selected < capacity {
        0
    } else {
        selected.saturating_add(1).saturating_sub(capacity)
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let titles = Tab::ALL.iter().enumerate().map(|(index, tab)| {
        Line::from(vec![Span::styled(
            format!(" {} {} ", index + 1, tab.title()),
            Style::default().fg(Color::White),
        )])
    });
    let selected = Tab::ALL.iter().position(|tab| *tab == app.tab).unwrap_or(0);
    let tabs = Tabs::new(titles)
        .select(selected)
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::BOTTOM).title(" omash "));
    frame.render_widget(tabs, area);
}

fn dashboard(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(5)])
        .split(area);
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 4); 4])
        .split(rows[0]);
    card(
        frame,
        cards[0],
        "STATUS",
        if app.online {
            "● ONLINE"
        } else if app.supervisor.running {
            "● STARTING"
        } else {
            "● OFFLINE"
        },
        if app.online { Color::Green } else { Color::Red },
    );
    card(
        frame,
        cards[1],
        "MODE",
        &app.snapshot.config.mode.to_uppercase(),
        ACCENT,
    );
    card(
        frame,
        cards[2],
        "TRAFFIC",
        &format!("↑ {}\n↓ {}", bytes(app.speeds.0), bytes(app.speeds.1)),
        Color::Cyan,
    );
    card(
        frame,
        cards[3],
        "CONNECTIONS",
        &app.snapshot.connections.connections.len().to_string(),
        Color::Yellow,
    );

    let info = vec![
        Line::from(vec![
            Span::styled("Core       ", Style::default().fg(MUTED)),
            Span::raw(value_or_dash(&app.snapshot.version.version)),
        ]),
        Line::from(vec![
            Span::styled("Supervisor ", Style::default().fg(MUTED)),
            Span::raw(if app.supervisor.running {
                format!(
                    "managed · pid {} · restarts {} · reloads {}",
                    app.supervisor
                        .pid
                        .map_or_else(|| "—".into(), |pid| pid.to_string()),
                    app.supervisor.restarts,
                    app.supervisor.reloads
                )
            } else {
                app.supervisor
                    .error
                    .clone()
                    .unwrap_or_else(|| "stopped".into())
            }),
        ]),
        Line::from(vec![
            Span::styled("Controller ", Style::default().fg(MUTED)),
            Span::raw(&app.config.controller),
        ]),
        Line::from(vec![
            Span::styled("Mixed port ", Style::default().fg(MUTED)),
            Span::raw(
                app.snapshot
                    .config
                    .mixed_port
                    .map_or("—".into(), |p| p.to_string()),
            ),
        ]),
        Line::from(vec![
            Span::styled("Allow LAN  ", Style::default().fg(MUTED)),
            Span::raw(bool_text(app.snapshot.config.allow_lan)),
        ]),
        Line::from(vec![
            Span::styled("IPv6       ", Style::default().fg(MUTED)),
            Span::raw(bool_text(app.snapshot.config.ipv6)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "s start/stop core · m cycle Rule → Global → Direct",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(info).block(Block::bordered().title(" Runtime ")),
        rows[1],
    );
}

fn profiles(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.profiles.items.iter().map(|profile| {
        let active = if app.profiles.current.as_deref() == Some(&profile.uid) {
            "●"
        } else {
            " "
        };
        let usage = profile
            .subscription
            .map(|sub| {
                let used = sub.upload.saturating_add(sub.download);
                if sub.total == 0 {
                    "—".into()
                } else {
                    format!("{} / {}", bytes(used), bytes(sub.total))
                }
            })
            .unwrap_or_else(|| "—".into());
        Row::new([
            active.into(),
            profile.name.clone(),
            format!("{:?}", profile.kind),
            usage,
            profile.updated.to_string(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Percentage(30),
            Constraint::Length(10),
            Constraint::Percentage(25),
            Constraint::Length(14),
        ],
    )
    .header(
        Row::new(["", "Name", "Type", "Subscription", "Updated"])
            .style(Style::default().fg(ACCENT)),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("› ")
    .block(
        Block::bordered().title(" Profiles  [a import · Enter activate · u update · D delete] "),
    );
    let mut state = TableState::default().with_selected(Some(app.profile_index));
    frame.render_stateful_widget(table, area, &mut state);
}

fn card(frame: &mut Frame, area: Rect, title: &str, value: &str, color: Color) {
    frame.render_widget(
        Paragraph::new(value)
            .alignment(Alignment::Center)
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
            .block(Block::bordered().title(format!(" {title} "))),
        area,
    );
}

fn proxies(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);
    let groups = app.proxy_groups();
    let group_items: Vec<_> = groups
        .iter()
        .map(|(name, proxy)| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{name:<18}")),
                Span::styled(format!("{:<10}", proxy.kind), Style::default().fg(MUTED)),
                Span::styled(&proxy.now, Style::default().fg(ACCENT)),
            ]))
        })
        .collect();
    let mut group_state = ListState::default().with_selected(Some(app.group_index));
    let group_border = if !app.node_focus { ACCENT } else { MUTED };
    frame.render_stateful_widget(
        List::new(group_items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(group_border))
                    .title(" Proxy groups "),
            ),
        columns[0],
        &mut group_state,
    );

    let nodes: Vec<_> = app
        .selected_group()
        .map(|(_, group)| {
            group
                .all
                .iter()
                .map(|name| {
                    let proxy = app.snapshot.proxies.proxies.get(name);
                    let delay = proxy
                        .and_then(|p| p.history.last())
                        .map_or_else(|| "—".into(), |h| format!("{} ms", h.delay));
                    let alive = proxy
                        .and_then(|p| p.alive)
                        .map_or("", |a| if a { "●" } else { "×" });
                    let active = if group.now == *name { "✓" } else { " " };
                    ListItem::new(format!("{active} {name:<32} {alive:<2} {delay}"))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut node_state = ListState::default().with_selected(Some(app.node_index));
    let node_border = if app.node_focus { ACCENT } else { MUTED };
    frame.render_stateful_widget(
        List::new(nodes)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(node_border))
                    .title(" Nodes  [Tab focus · Enter select · d delay · p providers] "),
            ),
        columns[1],
        &mut node_state,
    );
}

fn connections(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app
        .snapshot
        .connections
        .connections
        .iter()
        .map(|connection| {
            let target = if connection.metadata.host.is_empty() {
                &connection.metadata.destination_ip
            } else {
                &connection.metadata.host
            };
            Row::new(vec![
                Cell::from(format!(
                    "{}:{}",
                    target, connection.metadata.destination_port
                )),
                Cell::from(
                    format!(
                        "{} {}",
                        connection.metadata.network, connection.metadata.kind
                    )
                    .to_uppercase(),
                ),
                Cell::from(connection.chains.join(" → ")),
                Cell::from(format!(
                    "↑{} ↓{}",
                    bytes(connection.upload),
                    bytes(connection.download)
                )),
            ])
        });
    let widths = [
        Constraint::Percentage(38),
        Constraint::Length(8),
        Constraint::Percentage(32),
        Constraint::Percentage(22),
    ];
    let table = Table::new(rows, widths)
        .header(
            Row::new(["Destination", "Network", "Chain", "Traffic"])
                .style(Style::default().fg(ACCENT)),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ")
        .block(Block::bordered().title(" Active connections  [x close · X close all] "));
    let mut state = TableState::default().with_selected(Some(app.connection_index));
    frame.render_stateful_widget(table, area, &mut state);
}

fn rules(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.snapshot.rules.rules.iter().map(|rule| {
        Row::new([
            rule.kind.as_str(),
            rule.payload.as_str(),
            rule.proxy.as_str(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Percentage(62),
            Constraint::Percentage(25),
        ],
    )
    .header(Row::new(["Type", "Payload", "Policy"]).style(Style::default().fg(ACCENT)))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("› ")
    .block(Block::bordered().title(" Rules  [p update providers] "));
    let mut state = TableState::default().with_selected(Some(app.rule_index));
    frame.render_stateful_widget(table, area, &mut state);
}

fn logs(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<_> = app
        .logs
        .iter()
        .map(|line| ListItem::new(line.as_str()))
        .collect();
    frame.render_widget(
        List::new(items).block(Block::bordered().title(" Mihomo logs  [r refresh] ")),
        area,
    );
}

fn unlock(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.unlock_items.iter().map(|item| {
        Row::new([
            item.name.as_str(),
            item.status.as_str(),
            item.region.as_deref().unwrap_or("—"),
            item.check_time.as_deref().unwrap_or("—"),
        ])
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(30),
                Constraint::Percentage(30),
                Constraint::Length(12),
                Constraint::Percentage(25),
            ],
        )
        .header(
            Row::new(["Service", "Status", "Region", "Checked"]).style(Style::default().fg(ACCENT)),
        )
        .block(Block::bordered().title(" Media & AI unlock  [c check] ")),
        area,
    );
}

fn settings(frame: &mut Frame, app: &App, area: Rect) {
    let values = [
        (
            "Keep Mihomo running",
            on_off(crate::core::core_desired_enabled()),
        ),
        ("TUN mode", on_off(app.config.tun)),
        ("System proxy", on_off(app.config.system_proxy)),
        ("Allow LAN", on_off(app.config.allow_lan)),
        ("IPv6", on_off(app.config.ipv6)),
        ("Refresh interval", format!("{} ms", app.config.refresh_ms)),
        (
            "Update Mihomo core",
            format!("{} · Enter", value_or_dash(&app.snapshot.version.version)),
        ),
        ("Update GeoData", "Enter".into()),
    ];
    let items: Vec<_> = values
        .into_iter()
        .map(|(name, value)| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{name:<28}")),
                Span::styled(value, Style::default().fg(ACCENT)),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.setting_index));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .block(
                Block::bordered().title(" Settings  [Enter toggle · b backup · R restore latest] "),
            ),
        area,
        &mut state,
    );
}

fn help(frame: &mut Frame, _app: &App, area: Rect) {
    let text = vec![
        Line::styled(
            "Keyboard",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from("1–9 / ← →    switch page"),
        Line::from("j k / ↑ ↓    move selection"),
        Line::from("Tab           switch proxy pane"),
        Line::from("Enter         select proxy node"),
        Line::from("d             test selected node delay"),
        Line::from("m             cycle core mode"),
        Line::from("s             start/stop managed Mihomo"),
        Line::from("a/u/D         import/update/delete profile"),
        Line::from("x / X         close one / all connections"),
        Line::from("r             refresh now"),
        Line::from("q / Ctrl-C    quit"),
        Line::from(""),
        Line::styled(
            "Mouse",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from("click         switch page / select row"),
        Line::from("double-click  activate proxy/profile/setting"),
        Line::from("wheel         move selection"),
        Line::from("click cards   start/stop core or cycle mode"),
        Line::from("Settings      Enter runs core / GeoData updates"),
        Line::from(""),
        Line::styled(
            "Configuration",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::from("~/.config/omash/config.toml"),
        Line::from("Mihomo is installed and supervised exclusively by omash."),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Help ")),
        area,
    );
}

fn draw_input(frame: &mut Frame, app: &App) {
    let area = centered(70, 5, frame.area());
    frame.render_widget(Clear, area);
    let (title, content) = match app.input.as_ref() {
        Some(crate::app::InputMode::ImportProfile) => (
            " Import URL or local YAML path  [Enter · Esc] ",
            app.input_buffer.clone(),
        ),
        Some(crate::app::InputMode::RestoreBackup(path)) => (
            " Confirm restore  [y/N] ",
            format!("Overwrite current configuration with {}?", path.display()),
        ),
        None => return,
    };
    frame.render_widget(
        Paragraph::new(content).block(
            Block::bordered()
                .border_style(Style::default().fg(ACCENT))
                .title(title),
        ),
        area,
    );
}

fn centered(percent: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent) / 2),
        Constraint::Percentage(percent),
        Constraint::Percentage((100 - percent) / 2),
    ])
    .split(vertical[1])[1]
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let color = if app.online { MUTED } else { Color::Red };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {} ", app.status), Style::default().fg(color)),
            Span::styled("  q quit · ? help", Style::default().fg(MUTED)),
        ])),
        area,
    );
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", value, UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn bool_text(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "on",
        Some(false) => "off",
        None => "—",
    }
}
fn value_or_dash(value: &str) -> &str {
    if value.is_empty() { "—" } else { value }
}
fn on_off(value: bool) -> String {
    if value { "on".into() } else { "off".into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1536), "1.5 KiB");
    }

    #[test]
    fn keeps_selected_row_visible() {
        assert_eq!(visible_start(0, 20, 5), 0);
        assert_eq!(visible_start(4, 20, 5), 0);
        assert_eq!(visible_start(7, 20, 5), 3);
    }

    #[test]
    fn hit_region_excludes_right_and_bottom_edges() {
        let region = HitRegion {
            area: Rect::new(2, 3, 4, 2),
            target: HitTarget::CoreToggle,
        };
        assert!(region.contains(2, 3));
        assert!(region.contains(5, 4));
        assert!(!region.contains(6, 4));
        assert!(!region.contains(5, 5));
    }
}
