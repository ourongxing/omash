use crate::app::{App, Tab};
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, HighlightSpacing, List, ListItem, ListState, Padding,
        Paragraph, Row, Table, TableState, Tabs, Wrap,
    },
};

#[derive(Clone, Copy)]
struct ShellAreas {
    navigation: Rect,
    header: Rect,
    content: Rect,
    status: Rect,
    wide: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitTarget {
    Tab(Tab),
    CoreToggle,
    RoutingMode(&'static str),
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
    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme.background)),
        frame.area(),
    );
    let shell = shell_areas(frame.area());
    draw_navigation(frame, app, shell.navigation, shell.wide);
    draw_page_header(frame, app, shell.header);
    match app.tab {
        Tab::Dashboard => dashboard(frame, app, shell.content),
        Tab::Proxies => proxies(frame, app, shell.content),
        Tab::Profiles => profiles(frame, app, shell.content),
        Tab::Connections => connections(frame, app, shell.content),
        Tab::Rules => rules(frame, app, shell.content),
        Tab::Logs => logs(frame, app, shell.content),
        Tab::Unlock => unlock(frame, app, shell.content),
        Tab::Settings => settings(frame, app, shell.content),
        Tab::Help => help(frame, app, shell.content),
    }
    draw_status(frame, app, shell.status, shell.wide);
    if app.help_open {
        draw_help_overlay(frame, &app.theme);
    }
    if app.input.is_some() {
        draw_input(frame, app);
    }
    hit_regions(app, shell)
}

fn shell_areas(area: Rect) -> ShellAreas {
    let outer = area.inner(Margin::new(1, 0));
    if area.width >= 88 && area.height >= 24 {
        let rows = Layout::vertical([Constraint::Min(8), Constraint::Length(2)]).split(outer);
        let columns = Layout::horizontal([
            Constraint::Length(23),
            Constraint::Length(2),
            Constraint::Min(40),
        ])
        .split(rows[0]);
        let main = Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Min(4),
        ])
        .split(columns[2]);
        ShellAreas {
            navigation: columns[0],
            header: main[0],
            content: main[2].inner(Margin::new(1, 0)),
            status: rows[1],
            wide: true,
        }
    } else {
        let rows = Layout::vertical([Constraint::Min(8), Constraint::Length(3)]).split(outer);
        let main = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Min(4),
        ])
        .split(rows[0]);
        ShellAreas {
            navigation: main[0],
            header: main[1],
            content: main[3].inner(Margin::new(1, 0)),
            status: rows[1],
            wide: false,
        }
    }
}

fn hit_regions(app: &App, shell: ShellAreas) -> Vec<HitRegion> {
    let mut regions = tab_regions(shell.navigation, shell.wide);
    if shell.wide
        && let Some(buttons) = sidebar_mode_button_areas(shell.navigation)
    {
        regions.extend(buttons.into_iter().zip(["rule", "global", "direct"]).map(
            |(area, mode)| HitRegion {
                area,
                target: HitTarget::RoutingMode(mode),
            },
        ));
    }
    match app.tab {
        Tab::Dashboard => {
            let cards = dashboard_card_areas(shell.content);
            regions.push(HitRegion {
                area: cards[0],
                target: HitTarget::CoreToggle,
            });
        }
        Tab::Proxies => {
            let columns = proxy_columns(shell.content);
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
            shell.content,
            app.profiles.items.len(),
            app.profile_index,
            true,
            HitTarget::Profile,
        )),
        Tab::Connections => regions.extend(list_regions(
            shell.content,
            app.snapshot.connections.connections.len(),
            app.connection_index,
            true,
            HitTarget::Connection,
        )),
        Tab::Rules => regions.extend(list_regions(
            shell.content,
            app.snapshot.rules.rules.len(),
            app.rule_index,
            true,
            HitTarget::Rule,
        )),
        Tab::Settings => regions.extend(list_regions(
            shell.content,
            crate::app::SETTINGS_COUNT,
            app.setting_index,
            false,
            HitTarget::Setting,
        )),
        _ => {}
    }
    regions
}

fn tab_regions(area: Rect, wide: bool) -> Vec<HitRegion> {
    if wide {
        Tab::ALL
            .iter()
            .copied()
            .enumerate()
            .map(|(index, tab)| HitRegion {
                area: Rect::new(
                    area.x + 1,
                    area.y + 5 + index as u16 * 2,
                    area.width.saturating_sub(2),
                    1,
                ),
                target: HitTarget::Tab(tab),
            })
            .collect()
    } else {
        let tabs = Rect::new(area.x, area.y + 2, area.width, 2);
        let mut x = tabs.x.saturating_add(1);
        Tab::ALL
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(index, tab)| {
                let width = format!(" {} {} ", index + 1, short_title(tab))
                    .chars()
                    .count() as u16;
                let visible = width.min(tabs.right().saturating_sub(x));
                let region = (visible > 0).then_some(HitRegion {
                    area: Rect::new(x, tabs.y, visible, 1),
                    target: HitTarget::Tab(tab),
                });
                x = x.saturating_add(width).saturating_add(1);
                region
            })
            .collect()
    }
}

fn list_regions(
    area: Rect,
    len: usize,
    selected: usize,
    has_header: bool,
    target: fn(usize) -> HitTarget,
) -> Vec<HitRegion> {
    let inner = area.inner(Margin::new(1, 1));
    let header_height = if has_header { 2 } else { 0 };
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

fn draw_navigation(frame: &mut Frame, app: &App, area: Rect, wide: bool) {
    let selected = Tab::ALL.iter().position(|tab| *tab == app.tab).unwrap_or(0);
    if !wide {
        let areas = Layout::vertical([Constraint::Length(2), Constraint::Length(2)]).split(area);
        frame.render_widget(
            Paragraph::new(Line::styled(
                " O M A S H ",
                Style::default()
                    .fg(app.theme.foreground)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            areas[0],
        );
        let titles = Tab::ALL
            .iter()
            .enumerate()
            .map(|(index, tab)| Line::from(format!(" {} {} ", index + 1, short_title(*tab))));
        frame.render_widget(
            Tabs::new(titles)
                .select(selected)
                .divider(" ")
                .style(Style::default().fg(app.theme.muted))
                .highlight_style(
                    Style::default()
                        .fg(app.theme.accent)
                        .bg(app.theme.surface_active)
                        .add_modifier(Modifier::BOLD),
                ),
            areas[1],
        );
        return;
    }

    frame.render_widget(
        Block::default().style(Style::default().bg(app.theme.surface)),
        area,
    );
    let inner = area.inner(Margin::new(1, 1));
    let brand = Rect::new(inner.x, inner.y, inner.width, 3);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::styled(
                "█▀█ █▄ ▄█ █▀█ █▀▀ █ █",
                Style::default()
                    .fg(app.theme.foreground)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![Span::styled(
                "█▄█ █ ▀ █ █▀█ ▄▄█ █▀█",
                Style::default()
                    .fg(app.theme.foreground)
                    .add_modifier(Modifier::BOLD),
            )]),
        ])
        .alignment(Alignment::Center),
        brand,
    );

    for (index, tab) in Tab::ALL.iter().copied().enumerate() {
        let active = tab == app.tab;
        let row = Rect::new(
            area.x + 1,
            area.y + 5 + index as u16 * 2,
            area.width.saturating_sub(2),
            1,
        );
        let style = if active {
            Style::default()
                .fg(app.theme.accent)
                .bg(app.theme.surface_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.theme.foreground)
        };
        let marker = if active { "›" } else { " " };
        frame.render_widget(
            Paragraph::new(format!("{marker} {}  {}", index + 1, tab.title())).style(style),
            row,
        );
    }

    if area.height >= 28 {
        let state_area = Rect::new(
            area.x + 2,
            area.bottom() - 5,
            area.width.saturating_sub(4),
            5,
        );
        let (dot, label, color) = core_status(app);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![Span::styled(
                    format!("{dot} {label}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )]),
                Line::styled(
                    format!("  {}", app.status),
                    Style::default().fg(app.theme.muted),
                ),
            ]),
            state_area,
        );
        if let Some(buttons) = sidebar_mode_button_areas(area) {
            draw_mode_buttons(frame, buttons, &app.snapshot.config.mode, &app.theme);
        }
    }
}

fn draw_page_header(frame: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 {
        return;
    }
    let subtitle = match app.tab {
        Tab::Dashboard => "Live overview of your local proxy service",
        Tab::Proxies => "Choose routing groups and test node latency",
        Tab::Profiles => "Manage local and remote configuration profiles",
        Tab::Connections => "Inspect and close active network sessions",
        Tab::Rules => "Review the policies currently loaded by Mihomo",
        Tab::Logs => "Recent runtime output from the managed core",
        Tab::Unlock => "Check regional availability for media and AI services",
        Tab::Settings => "Core behavior, networking and application maintenance",
        Tab::Help => "Keyboard and mouse shortcuts",
    };
    let area = area.inner(Margin::new(1, 0));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::styled(
                app.tab.title(),
                Style::default()
                    .fg(app.theme.foreground)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(subtitle, Style::default().fg(app.theme.muted)),
        ])
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(app.theme.border)),
        ),
        area,
    );
}

fn short_title(tab: Tab) -> &'static str {
    match tab {
        Tab::Dashboard => "Home",
        Tab::Proxies => "Proxy",
        Tab::Profiles => "Profiles",
        Tab::Connections => "Conns",
        Tab::Rules => "Rules",
        Tab::Logs => "Logs",
        Tab::Unlock => "Unlock",
        Tab::Settings => "Settings",
        Tab::Help => "Help",
    }
}

fn dashboard(frame: &mut Frame, app: &App, area: Rect) {
    let cards = dashboard_card_areas(area);
    card(
        frame,
        cards[0],
        "CORE STATUS",
        if app.online {
            "●  Online"
        } else if app.supervisor.running {
            "◐  Starting"
        } else {
            "●  Offline"
        },
        if app.online {
            app.theme.success
        } else {
            app.theme.danger
        },
        &app.theme,
    );
    card(
        frame,
        cards[1],
        "ROUTING MODE",
        &app.snapshot.config.mode.to_uppercase(),
        app.theme.accent,
        &app.theme,
    );
    card(
        frame,
        cards[2],
        "LIVE TRAFFIC",
        &format!("↑ {}   ↓ {}", bytes(app.speeds.0), bytes(app.speeds.1)),
        app.theme.info,
        &app.theme,
    );
    card(
        frame,
        cards[3],
        "SESSIONS",
        &app.snapshot.connections.connections.len().to_string(),
        app.theme.warning,
        &app.theme,
    );

    let card_height = if area.width >= 72 { 6 } else { 11 };
    let details_area = Rect::new(
        area.x,
        area.y.saturating_add(card_height),
        area.width,
        area.height.saturating_sub(card_height),
    );
    let details = if details_area.width >= 70 {
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
            .spacing(2)
            .split(details_area)
    } else {
        Layout::horizontal([Constraint::Percentage(100), Constraint::Length(0)]).split(details_area)
    };
    let info = vec![
        Line::from(vec![
            Span::styled("VERSION       ", Style::default().fg(app.theme.muted)),
            Span::styled(
                value_or_dash(&app.snapshot.version.version),
                Style::default().fg(app.theme.foreground),
            ),
        ]),
        Line::from(vec![
            Span::styled("SUPERVISOR    ", Style::default().fg(app.theme.muted)),
            Span::raw(if app.supervisor.running {
                format!(
                    "Managed · PID {} · {} restarts · {} reloads",
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
            Span::styled("CONTROLLER    ", Style::default().fg(app.theme.muted)),
            Span::raw(&app.config.controller),
        ]),
        Line::from(vec![
            Span::styled("MIXED PORT    ", Style::default().fg(app.theme.muted)),
            Span::raw(
                app.snapshot
                    .config
                    .mixed_port
                    .map_or("—".into(), |p| p.to_string()),
            ),
        ]),
        Line::from(vec![
            Span::styled("ALLOW LAN     ", Style::default().fg(app.theme.muted)),
            status_badge(bool_text(app.snapshot.config.allow_lan), &app.theme),
        ]),
        Line::from(vec![
            Span::styled("IPV6          ", Style::default().fg(app.theme.muted)),
            status_badge(bool_text(app.snapshot.config.ipv6), &app.theme),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(info).block(panel(" Runtime ", &app.theme)),
        details[0],
    );

    if details.len() > 1 && details[1].width > 0 {
        let selected = app
            .selected_group()
            .map(|(name, group)| {
                vec![
                    Line::styled("CURRENT ROUTE", Style::default().fg(app.theme.muted)),
                    Line::styled(
                        name,
                        Style::default()
                            .fg(app.theme.foreground)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Line::from(""),
                    Line::styled("SELECTED NODE", Style::default().fg(app.theme.muted)),
                    Line::styled(
                        value_or_dash(&group.now),
                        Style::default()
                            .fg(app.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Line::from(""),
                    Line::styled(
                        "Open Proxies to switch or test nodes.",
                        Style::default().fg(app.theme.muted),
                    ),
                ]
            })
            .unwrap_or_else(|| {
                vec![Line::styled(
                    "No proxy group available",
                    Style::default().fg(app.theme.muted),
                )]
            });
        frame.render_widget(
            Paragraph::new(selected)
                .wrap(Wrap { trim: true })
                .block(panel(" Active route ", &app.theme)),
            details[1],
        );
    }
}

fn dashboard_card_areas(area: Rect) -> Vec<Rect> {
    if area.width >= 72 {
        Layout::horizontal([Constraint::Ratio(1, 4); 4])
            .spacing(2)
            .split(Rect::new(area.x, area.y, area.width, area.height.min(5)))
            .iter()
            .copied()
            .collect()
    } else {
        let rows = Layout::vertical([Constraint::Length(5), Constraint::Length(5)])
            .split(Rect::new(area.x, area.y, area.width, area.height.min(10)));
        let top = Layout::horizontal([Constraint::Ratio(1, 2); 2])
            .spacing(2)
            .split(rows[0]);
        let bottom = Layout::horizontal([Constraint::Ratio(1, 2); 2])
            .spacing(2)
            .split(rows[1]);
        vec![top[0], top[1], bottom[0], bottom[1]]
    }
}

fn sidebar_mode_button_areas(area: Rect) -> Option<[Rect; 3]> {
    if area.height < 28 {
        return None;
    }
    let row = Rect::new(
        area.x + 3,
        area.bottom() - 2,
        area.width.saturating_sub(6),
        1,
    );
    let columns = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(row);
    Some([columns[0], columns[1], columns[2]])
}

fn draw_mode_buttons(frame: &mut Frame, buttons: [Rect; 3], current: &str, theme: &Theme) {
    for ((button, mode), label) in buttons
        .into_iter()
        .zip(["rule", "global", "direct"])
        .zip(["RULE", "GLOBAL", "DIRECT"])
    {
        let active = current.eq_ignore_ascii_case(mode);
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(if active {
                    Style::default()
                        .fg(theme.background)
                        .bg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.muted).bg(theme.surface_active)
                }),
            button,
        );
    }
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
            .style(
                Style::default()
                    .fg(app.theme.muted)
                    .add_modifier(Modifier::BOLD),
            )
            .bottom_margin(1),
    )
    .row_highlight_style(selection_style(true, &app.theme))
    .highlight_symbol("▎ ")
    .block(panel(" Profiles ", &app.theme));
    let mut state = TableState::default().with_selected(Some(app.profile_index));
    frame.render_stateful_widget(table, area, &mut state);
}

fn card(frame: &mut Frame, area: Rect, title: &str, value: &str, color: Color, theme: &Theme) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::styled(
                title,
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                value,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme.surface)),
        area,
    );
}

fn inset_panel(area: Rect) -> Rect {
    if area.width > 4 {
        area.inner(Margin::new(1, 0))
    } else {
        area
    }
}

fn panel<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::default()
        .style(Style::default().bg(theme.surface))
        .padding(Padding::new(1, 1, 1, 1))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ))
}

fn focus_panel<'a>(title: &'a str, focused: bool, theme: &Theme) -> Block<'a> {
    Block::default()
        .style(Style::default().bg(theme.surface))
        .padding(Padding::new(1, 1, 1, 1))
        .title(Span::styled(
            title,
            Style::default()
                .fg(if focused { theme.accent } else { theme.muted })
                .add_modifier(Modifier::BOLD),
        ))
}

fn selection_style(focused: bool, theme: &Theme) -> Style {
    if focused {
        Style::default()
            .fg(theme.foreground)
            .bg(theme.surface_active)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    }
}

fn status_badge(value: &'static str, theme: &Theme) -> Span<'static> {
    let color = if value == "on" {
        theme.success
    } else {
        theme.muted
    };
    Span::styled(
        value.to_uppercase(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn proxies(frame: &mut Frame, app: &App, area: Rect) {
    let columns = proxy_columns(area);
    let groups = app.proxy_groups();
    let row_width = columns[0].width.saturating_sub(6) as usize;
    let proxy_width = 10.min(row_width.saturating_sub(2) / 2);
    let name_width = row_width.saturating_sub(proxy_width + 1);
    let group_items: Vec<_> = groups
        .iter()
        .map(|(name, proxy)| {
            ListItem::new(Line::from(vec![
                Span::raw(fit_column(name, name_width, false)),
                Span::raw(" "),
                Span::styled(
                    fit_column(&proxy.now, proxy_width, true),
                    Style::default().fg(app.theme.muted),
                ),
            ]))
        })
        .collect();
    let mut group_state = ListState::default().with_selected(Some(app.group_index));
    let group_focused = !app.node_focus;
    frame.render_stateful_widget(
        List::new(group_items)
            .highlight_symbol("▎ ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(selection_style(group_focused, &app.theme))
            .block(focus_panel(" Proxy groups ", group_focused, &app.theme)),
        columns[0],
        &mut group_state,
    );

    let node_row_width = columns[1].width.saturating_sub(6) as usize;
    let delay_width = 8.min(node_row_width.saturating_sub(6) / 3);
    let node_name_width = node_row_width.saturating_sub(delay_width + 5);
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
                    let (alive, alive_color) = match proxy.and_then(|p| p.alive) {
                        Some(true) => ("●", app.theme.success),
                        Some(false) => ("×", app.theme.danger),
                        None => (" ", app.theme.muted),
                    };
                    let active = if group.now == *name { "✓" } else { " " };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{active} "), Style::default().fg(app.theme.accent)),
                        Span::raw(fit_column(name, node_name_width, false)),
                        Span::raw(" "),
                        Span::styled(alive, Style::default().fg(alive_color)),
                        Span::raw(" "),
                        Span::styled(
                            fit_column(&delay, delay_width, true),
                            Style::default().fg(app.theme.muted),
                        ),
                    ]))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut node_state = ListState::default().with_selected(Some(app.node_index));
    let node_focused = app.node_focus;
    let node_title = match app.selected_group() {
        Some((_, group)) if !group.kind.eq_ignore_ascii_case("selector") => " Nodes · automatic ",
        _ => " Nodes ",
    };
    frame.render_stateful_widget(
        List::new(nodes)
            .highlight_symbol("▎ ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(selection_style(node_focused, &app.theme))
            .block(focus_panel(node_title, node_focused, &app.theme)),
        columns[1],
        &mut node_state,
    );
}

fn fit_column(value: &str, width: usize, align_right: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let value_width = Line::from(value).width();
    if value_width <= width {
        let padding = " ".repeat(width - value_width);
        return if align_right {
            format!("{padding}{value}")
        } else {
            format!("{value}{padding}")
        };
    }
    if width == 1 {
        return "…".into();
    }

    let content_width = width - 1;
    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = Line::from(character.to_string()).width();
        if used + character_width > content_width {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result.push_str(&" ".repeat(content_width - used));
    result
}

fn proxy_columns(area: Rect) -> Vec<Rect> {
    Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
        .spacing(2)
        .split(area)
        .iter()
        .copied()
        .collect()
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
                .style(
                    Style::default()
                        .fg(app.theme.muted)
                        .add_modifier(Modifier::BOLD),
                )
                .bottom_margin(1),
        )
        .row_highlight_style(selection_style(true, &app.theme))
        .highlight_symbol("▎ ")
        .block(panel(" Active connections ", &app.theme));
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
    .header(
        Row::new(["Type", "Payload", "Policy"])
            .style(
                Style::default()
                    .fg(app.theme.muted)
                    .add_modifier(Modifier::BOLD),
            )
            .bottom_margin(1),
    )
    .row_highlight_style(selection_style(true, &app.theme))
    .highlight_symbol("▎ ")
    .block(panel(" Rules ", &app.theme));
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
        List::new(items).block(panel(" Mihomo logs ", &app.theme)),
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
            Row::new(["Service", "Status", "Region", "Checked"])
                .style(
                    Style::default()
                        .fg(app.theme.muted)
                        .add_modifier(Modifier::BOLD),
                )
                .bottom_margin(1),
        )
        .block(panel(" Media & AI unlock ", &app.theme)),
        area,
    );
}

fn settings(frame: &mut Frame, app: &App, area: Rect) {
    let values = [
        (
            "Keep Mihomo running",
            on_off(crate::core::core_desired_enabled()),
        ),
        ("Start on login", on_off(app.config.auto_start)),
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
                Span::styled(value, Style::default().fg(app.theme.accent)),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.setting_index));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("▎ ")
            .highlight_style(selection_style(true, &app.theme))
            .block(panel(" Settings ", &app.theme)),
        area,
        &mut state,
    );
}

fn help(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(panel(" Keyboard shortcuts ", &app.theme), area);
    render_help_columns(frame, area.inner(Margin::new(2, 2)), &app.theme);
}

fn draw_help_overlay(frame: &mut Frame, theme: &Theme) {
    let height = frame.area().height.saturating_sub(4).clamp(8, 22);
    let area = centered(76, height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        panel(" Keyboard shortcuts  ·  Esc to close ", theme)
            .border_style(Style::default().fg(theme.accent)),
        area,
    );
    render_help_columns(frame, area.inner(Margin::new(2, 2)), theme);
}

fn render_help_columns(frame: &mut Frame, area: Rect, theme: &Theme) {
    let columns = Layout::horizontal([Constraint::Ratio(1, 2); 2]).split(area);
    let navigation = vec![
        section_line("NAVIGATION", theme),
        help_binding("1–9", "Open page", theme),
        help_binding("↑↓ / jk", "Move selection", theme),
        help_binding("Tab", "Switch proxy pane", theme),
        help_binding("←→ / hl", "Switch proxy pane", theme),
        Line::from(""),
        section_line("GLOBAL", theme),
        help_binding("r", "Refresh data", theme),
        help_binding("?", "Toggle shortcuts", theme),
        help_binding("Esc", "Close dialog", theme),
        help_binding("q", "Quit", theme),
        help_binding("Ctrl-C", "Quit", theme),
    ];
    let actions = vec![
        section_line("CONTEXTUAL ACTIONS", theme),
        help_binding("Enter", "Activate selection", theme),
        help_binding("s", "Start / stop core", theme),
        help_binding("m", "Cycle routing mode", theme),
        help_binding("d", "Test node delay", theme),
        help_binding("a", "Import profile", theme),
        help_binding("u", "Update profile", theme),
        help_binding("D", "Delete profile", theme),
        help_binding("x / X", "Close one / all connections", theme),
        help_binding("p", "Update providers", theme),
        help_binding("c", "Run unlock checks", theme),
        help_binding("b / R", "Backup / restore", theme),
        Line::from(""),
        section_line("MOUSE", theme),
        help_binding("Click", "Focus item", theme),
        help_binding("Double", "Activate item", theme),
        help_binding("Wheel", "Move selection", theme),
    ];
    frame.render_widget(Paragraph::new(navigation), inset_panel(columns[0]));
    frame.render_widget(Paragraph::new(actions), inset_panel(columns[1]));
}

fn section_line(title: &'static str, theme: &Theme) -> Line<'static> {
    Line::styled(
        title,
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::BOLD),
    )
}

fn help_binding(key: &'static str, description: &'static str, theme: &Theme) -> Line<'static> {
    let key_color = if matches!(key, "D" | "X" | "R") {
        theme.danger
    } else {
        theme.accent
    };
    Line::from(vec![
        Span::styled(
            format!(" {key:<9}"),
            Style::default()
                .fg(key_color)
                .bg(theme.surface_active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {description}"),
            Style::default().fg(theme.foreground),
        ),
    ])
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
        Paragraph::new(content)
            .style(Style::default().bg(app.theme.surface))
            .block(
                Block::default()
                    .style(Style::default().bg(app.theme.surface))
                    .padding(Padding::new(2, 2, 2, 1))
                    .title(Span::styled(
                        title,
                        Style::default()
                            .fg(app.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    )),
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

fn core_status(app: &App) -> (&'static str, &'static str, Color) {
    if app.online {
        ("●", "CORE ONLINE", app.theme.success)
    } else if app.supervisor.running {
        ("◐", "CORE STARTING", app.theme.warning)
    } else {
        ("●", "CORE OFFLINE", app.theme.danger)
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect, wide: bool) {
    frame.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(app.theme.border)),
        area,
    );
    if area.height < 2 {
        return;
    }
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(1),
    );
    if wide {
        frame.render_widget(
            Paragraph::new(shortcut_line(app, inner.width, &app.theme)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    } else {
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);
        let (dot, core_label, color) = core_status(app);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(
                    core_label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" · ", Style::default().fg(app.theme.muted)),
                Span::styled(&app.status, Style::default().fg(app.theme.foreground)),
            ])),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(shortcut_line(app, inner.width, &app.theme)),
            rows[1],
        );
    }
}

fn contextual_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    match app.tab {
        Tab::Dashboard => &[("s", "Core"), ("m", "Mode")],
        Tab::Proxies if app.selected_group_is_manual() => &[
            ("Tab", "Pane"),
            ("Enter", "Select"),
            ("d", "Delay"),
            ("p", "Update"),
        ],
        Tab::Proxies => &[("Tab", "Pane"), ("d", "Delay"), ("p", "Update")],
        Tab::Profiles => &[
            ("Enter", "Activate"),
            ("a", "Import"),
            ("u", "Update"),
            ("D", "Delete"),
        ],
        Tab::Connections => &[("x", "Close"), ("X", "Close all")],
        Tab::Rules => &[("p", "Update")],
        Tab::Logs => &[("r", "Refresh")],
        Tab::Unlock => &[("c", "Check")],
        Tab::Settings => &[("Enter", "Change"), ("b", "Backup"), ("R", "Restore")],
        Tab::Help => &[],
    }
}

fn shortcut_line(app: &App, width: u16, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    let reserved = 19usize;
    for &(key, description) in contextual_hints(app) {
        let size = key.chars().count() + description.chars().count() + 5;
        if used + size + reserved > width as usize {
            break;
        }
        push_hint(&mut spans, key, description, theme);
        used += size;
    }
    push_hint(&mut spans, "?", "Help", theme);
    push_hint(&mut spans, "q", "Quit", theme);
    Line::from(spans)
}

fn push_hint(
    spans: &mut Vec<Span<'static>>,
    key: &'static str,
    description: &'static str,
    theme: &Theme,
) {
    let key_color = if matches!(key, "D" | "X" | "R") {
        theme.danger
    } else {
        theme.accent
    };
    spans.push(Span::styled(
        format!(" {key} "),
        Style::default()
            .fg(key_color)
            .bg(theme.surface_active)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!(" {description}  "),
        Style::default().fg(theme.muted),
    ));
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
    fn fits_proxy_group_columns_by_display_width() {
        assert_eq!(fit_column("Group", 7, false), "Group  ");
        assert_eq!(fit_column("Node", 7, true), "   Node");
        assert_eq!(fit_column("节点选择", 6, false), "节点… ");
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
