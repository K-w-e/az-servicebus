use crate::app::{App, AppFocus, HitAreas, PendingAction};
use crate::azure::Message;
use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Cyan;

const STATUS_PREFIX: &str = " Status: ";

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hit = HitAreas::default();

    // La barra di stato cresce (fino a 4 righe) per mostrare messaggi lunghi.
    let width = f.area().width.max(1) as usize;
    let status_len = STATUS_PREFIX.chars().count() + 2 + app.status.chars().count();
    let status_h = status_len.div_ceil(width).clamp(1, 4) as u16;

    let [header, top, detail, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Percentage(45),
        Constraint::Min(6),
        Constraint::Length(status_h),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, app, header);
    draw_status(f, app, status);

    let [topics, subs, msgs] = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(30),
        Constraint::Percentage(45),
    ])
    .areas(top);

    let focus = match app.focus {
        AppFocus::ProfilesModal | AppFocus::DeleteConfirmModal => app.base_focus,
        f => f,
    };

    app.hit.topics = topics;
    app.hit.subs = subs;
    app.hit.messages = msgs;
    app.hit.detail = detail;

    draw_topics(f, app, topics, focus == AppFocus::TopicsList);
    draw_subs(f, app, subs, focus == AppFocus::SubscriptionsList);
    draw_messages(f, app, msgs, focus == AppFocus::MessagesList);
    draw_detail(f, app, detail, focus == AppFocus::MessageDetail);
    draw_footer(f, app, footer);

    match app.focus {
        AppFocus::ProfilesModal => draw_profiles(f, app),
        AppFocus::DeleteConfirmModal => draw_confirm(f, app),
        _ => {}
    }
}

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Block::new()
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(style)
        .title(title)
}

fn highlight() -> Style {
    Style::new()
        .bg(Color::Rgb(40, 60, 80))
        .add_modifier(Modifier::BOLD)
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let profile = app.profile_name.clone().unwrap_or_else(|| "-".into());
    let host = app.host.clone().unwrap_or_default();
    let spans = vec![
        Span::styled(" az-servicebus ", Style::new().bg(ACCENT).fg(Color::Black).bold()),
        Span::raw(" │ Profile: "),
        Span::styled(
            format!("[{profile}]"),
            Style::new().fg(Color::Yellow).bold(),
        ),
        Span::styled(format!(" {host}"), Style::new().fg(Color::DarkGray)),
        Span::raw(" │ Mouse: "),
        Span::styled(
            if app.mouse_enabled { "on" } else { "off" },
            Style::new().fg(Color::Gray),
        ),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let status_style = if app.status_error {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::Green)
    };
    let mut spans = vec![Span::styled(STATUS_PREFIX, Style::new().bold())];
    if app.is_busy() {
        spans.push(Span::styled(
            format!("{} ", SPINNER[app.spinner % SPINNER.len()]),
            Style::new().fg(ACCENT),
        ));
    }
    spans.push(Span::styled(app.status.clone(), status_style));
    f.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_topics(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let items: Vec<ListItem> = app
        .topics
        .iter()
        .map(|t| ListItem::new(t.as_str()))
        .collect();
    let title = format!(" Topics ({}) ", app.topics.len());
    let list = List::new(items)
        .block(panel(title, focused))
        .highlight_style(highlight())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.topics_state);
}

fn draw_subs(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let items: Vec<ListItem> = app
        .subs
        .iter()
        .map(|s| {
            let mut spans = vec![
                Span::raw(s.name.clone()),
                Span::styled(
                    format!(" ({})", s.active_count),
                    Style::new().fg(Color::Gray),
                ),
            ];
            if s.dead_letter_count > 0 {
                spans.push(Span::styled(
                    format!(" {} DLQ", s.dead_letter_count),
                    Style::new().fg(Color::Red).bold(),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let title = format!(" Subscriptions ({}) ", app.subs.len());
    let list = List::new(items)
        .block(panel(title, focused))
        .highlight_style(highlight())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.subs_state);
}

fn draw_messages(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let mode = if app.dlq {
        Span::styled("DLQ", Style::new().fg(Color::Red).bold())
    } else {
        Span::styled("Active", Style::new().fg(Color::Green).bold())
    };
    let title = Line::from(vec![
        Span::raw(" Messages ("),
        mode,
        Span::raw(format!(
            ") {} [max {}] ",
            app.messages.len(),
            app.max_messages
        )),
    ]);
    let block = panel(title, focused);

    if app.messages.is_empty() {
        let hint = if app.messages_busy {
            "Caricamento…"
        } else if app.selected_sub().is_none() {
            "Seleziona una subscription"
        } else if app.messages_key.is_some() {
            "Nessun messaggio"
        } else {
            "[Enter] / [r] per fare peek dei messaggi\n[t] per alternare Active / DLQ"
        };
        let p = Paragraph::new(hint)
            .fg(Color::DarkGray)
            .alignment(Alignment::Center)
            .block(block);
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .messages
        .iter()
        .map(|m| {
            let tag = m.label.clone().unwrap_or_else(|| m.message_id.clone());
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("[{}] ", m.sequence_number),
                    Style::new().fg(Color::Yellow),
                ),
                Span::raw(m.enqueued.clone()),
                Span::styled(format!("  {tag}"), Style::new().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.messages_state);
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let block = panel(" Message Details ", focused);
    let Some(m) = app.selected_message() else {
        f.render_widget(Paragraph::new("").block(block), area);
        return;
    };
    let p = Paragraph::new(detail_text(m))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    f.render_widget(p, area);
}

fn detail_text(m: &Message) -> Text<'static> {
    let key = |s: &str| Span::styled(s.to_string(), Style::new().fg(ACCENT).bold());
    let sep = || Span::styled(" │ ", Style::new().fg(Color::DarkGray));
    let mut lines = vec![
        Line::from(vec![
            key("Sequence: "),
            Span::raw(m.sequence_number.to_string()),
            sep(),
            key("MessageId: "),
            Span::raw(m.message_id.clone()),
            sep(),
            key("Enqueued: "),
            Span::raw(format!("{} UTC", m.enqueued)),
        ]),
        Line::from(vec![
            key("DeliveryCount: "),
            Span::raw(m.delivery_count.to_string()),
            sep(),
            key("Label: "),
            Span::raw(m.label.clone().unwrap_or_else(|| "-".into())),
            sep(),
            key("ContentType: "),
            Span::raw(m.content_type.clone().unwrap_or_else(|| "-".into())),
        ]),
    ];

    if !m.user_properties.is_empty() {
        lines.push(Line::from(key("User Properties")));
        for (k, v) in &m.user_properties {
            lines.push(Line::from(vec![
                Span::styled(format!("  {k}: "), Style::new().fg(Color::Magenta)),
                Span::raw(v.clone()),
            ]));
        }
    }
    let broker: Vec<String> = m
        .broker_properties
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if !broker.is_empty() {
        lines.push(Line::from(vec![
            key("Broker: "),
            Span::styled(broker.join("  "), Style::new().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "─".repeat(60),
        Style::new().fg(Color::DarkGray),
    )));

    let body = match serde_json::from_str::<serde_json::Value>(&m.body) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| m.body.clone()),
        Err(_) => m.body.clone(),
    };
    for l in body.lines() {
        lines.push(Line::from(l.replace('\t', "    ")));
    }
    Text::from(lines)
}

/// Tasto simulato dal click su un'etichetta del footer.
fn footer_code(k: &str) -> Option<KeyCode> {
    match k {
        "Tab" => Some(KeyCode::Tab),
        "Enter" => Some(KeyCode::Enter),
        "Esc" => Some(KeyCode::Esc),
        s if s.chars().count() == 1 => s.chars().next().map(KeyCode::Char),
        _ => None,
    }
}

fn draw_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let keys: &[(&str, &str)] = match app.focus {
        AppFocus::ProfilesModal if app.profile_form.is_some() => &[
            ("Tab", "Campo"),
            ("Enter", "Salva"),
            ("Ctrl-U", "Pulisci"),
            ("Esc", "Annulla"),
        ],
        AppFocus::ProfilesModal => &[
            ("j/k", "Navigate"),
            ("Enter", "Attiva"),
            ("a", "Aggiungi"),
            ("x", "Rimuovi"),
            ("Esc", "Chiudi"),
        ],
        AppFocus::DeleteConfirmModal => &[("y", "Conferma"), ("N", "Annulla")],
        AppFocus::MessageDetail => &[
            ("Tab", "Focus"),
            ("j/k", "Scroll"),
            ("t", "Active/DLQ"),
            ("d", "Delete"),
            ("r", "Refresh"),
            ("m", "Mouse"),
            ("Esc", "Back"),
            ("q", "Quit"),
        ],
        _ => &[
            ("Tab", "Focus"),
            ("j/k", "Navigate"),
            ("Enter", "Open"),
            ("t", "Active/DLQ"),
            ("p", "Profiles"),
            ("d", "Delete"),
            ("P", "Purge"),
            ("+/-", "Max"),
            ("r", "Refresh"),
            ("m", "Mouse"),
            ("q", "Quit"),
        ],
    };
    let mut spans = vec![];
    let mut x = area.x;
    for (k, label) in keys {
        let key = format!(" [{k}]");
        let label = format!(" {label}");
        let w = (key.chars().count() + label.chars().count()) as u16;
        if let Some(code) = footer_code(k) {
            let r = Rect::new(x, area.y, w, 1).intersection(area);
            app.hit.footer_keys.push((r, code));
        }
        x = x.saturating_add(w);
        spans.push(Span::styled(key, Style::new().fg(ACCENT).bold()));
        spans.push(Span::raw(label));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w)])
        .flex(Flex::Center)
        .areas(v);
    r
}

fn modal_block(title: &str, color: Color) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(color))
        .title(format!(" {title} "))
        .title_alignment(Alignment::Center)
}

fn draw_profiles(f: &mut Frame, app: &mut App) {
    let width = f.area().width.saturating_sub(4).min(90);
    if let Some(form) = &app.profile_form {
        let area = centered(f.area(), width, 11);
        f.render_widget(Clear, area);
        app.hit.modal = area;
        // Righe 2 e 3 dell'area interna: campi Nome e Connection string.
        let row = |n: u16| Rect::new(area.x + 1, area.y + 1 + n, area.width.saturating_sub(2), 1);
        app.hit.form_fields = [row(2), row(3)];
        let inner_w = width.saturating_sub(16) as usize;
        let field = |label: &str, value: &str, active: bool, secret: bool| {
            let shown: String = if secret && !active && !value.is_empty() {
                "••••••••".into()
            } else {
                let n = value.chars().count();
                value.chars().skip(n.saturating_sub(inner_w)).collect()
            };
            let style = if active {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            Line::from(vec![
                Span::styled(format!("{label:<12}"), Style::new().fg(ACCENT).bold()),
                Span::styled(format!("{shown}{}", if active { "▏" } else { "" }), style),
            ])
        };
        let mut lines = vec![
            Line::from("Nuovo profilo di connessione").bold(),
            Line::from(""),
            field("Nome", &form.name, form.field == 0, false),
            field("Conn. str.", &form.conn, form.field == 1, true),
            Line::from(""),
            Line::from("Endpoint=sb://<ns>.servicebus.windows.net/;SharedAccessKeyName=…;SharedAccessKey=…")
                .fg(Color::DarkGray),
        ];
        if let Some(e) = &form.error {
            lines.push(Line::from(e.clone()).fg(Color::Red));
        }
        f.render_widget(
            Paragraph::new(lines)
                .block(modal_block("Profiles", ACCENT))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let h = (app.config.profiles.len() as u16 + 4).clamp(6, 20);
    let area = centered(f.area(), width.min(60), h);
    f.render_widget(Clear, area);
    app.hit.modal = area;
    let active = app.config.active_profile.clone();
    let items: Vec<ListItem> = app
        .config
        .profiles
        .iter()
        .map(|p| {
            let host = crate::azure::ConnectionInfo::parse(&p.connection_string)
                .map(|c| c.host)
                .unwrap_or_else(|_| "connection string non valida".into());
            let marker = if active.as_deref() == Some(p.name.as_str()) {
                "● "
            } else {
                "  "
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::new().fg(Color::Green)),
                Span::styled(p.name.clone(), Style::new().bold()),
                Span::styled(format!("  {host}"), Style::new().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(modal_block("Profiles", ACCENT))
        .highlight_style(highlight())
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut app.profiles_state);
}

fn draw_confirm(f: &mut Frame, app: &mut App) {
    let text = match &app.pending_action {
        Some(PendingAction::DeleteMessage {
            topic,
            sub,
            dlq,
            seq,
        }) => vec![
            Line::from(format!("Eliminare definitivamente il messaggio {seq}?")).bold(),
            Line::from(format!("{topic}/{sub}{}", if *dlq { " (DLQ)" } else { "" }))
                .fg(Color::Gray),
        ],
        Some(PendingAction::Purge { topic, sub, dlq }) => vec![
            Line::from(format!(
                "Eliminare TUTTI i messaggi {}?",
                if *dlq {
                    "della Dead-Letter Queue"
                } else {
                    "attivi"
                }
            ))
            .bold(),
            Line::from(format!("{topic}/{sub}")).fg(Color::Gray),
        ],
        None => vec![],
    };
    let mut lines = text;
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("Operazione irreversibile. Confermi? "),
        Span::styled("[y/N]", Style::new().fg(Color::Yellow).bold()),
    ]));
    let area = centered(f.area(), f.area().width.saturating_sub(4).min(64), 9);
    f.render_widget(Clear, area);
    app.hit.modal = area;
    let block = modal_block("Conferma", Color::Red);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [text_area, buttons] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        text_area,
    );

    let yes = " Sì, elimina (y) ";
    let no = " Annulla (N) ";
    let [b_yes, b_no] = Layout::horizontal([
        Constraint::Length(yes.chars().count() as u16),
        Constraint::Length(no.chars().count() as u16),
    ])
    .flex(Flex::Center)
    .spacing(4)
    .areas(buttons);
    f.render_widget(
        Paragraph::new(yes).style(Style::new().bg(Color::Red).fg(Color::White).bold()),
        b_yes,
    );
    f.render_widget(
        Paragraph::new(no).style(Style::new().bg(Color::DarkGray).fg(Color::White)),
        b_no,
    );
    app.hit.confirm_yes = b_yes;
    app.hit.confirm_no = b_no;
}
