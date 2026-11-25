use crate::azure::{ConnectionInfo, Message, SubscriptionInfo};
use crate::config::{Config, Profile};
use crate::worker::{Command, Event};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

pub const DEFAULT_MAX_MESSAGES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppFocus {
    ProfilesModal,
    TopicsList,
    SubscriptionsList,
    MessagesList,
    MessageDetail,
    DeleteConfirmModal,
}

#[derive(Debug, Clone)]
pub enum PendingAction {
    DeleteMessage {
        topic: String,
        sub: String,
        dlq: bool,
        seq: i64,
    },
    Purge {
        topic: String,
        sub: String,
        dlq: bool,
    },
}

/// Aree cliccabili, aggiornate dalla UI a ogni render.
#[derive(Debug, Default, Clone)]
pub struct HitAreas {
    pub topics: Rect,
    pub subs: Rect,
    pub messages: Rect,
    pub detail: Rect,
    pub modal: Rect,
    pub form_fields: [Rect; 2],
    pub confirm_yes: Rect,
    pub confirm_no: Rect,
    pub footer_keys: Vec<(Rect, KeyCode)>,
}

#[derive(Debug, Default)]
pub struct ProfileForm {
    pub name: String,
    pub conn: String,
    pub field: usize,
    pub error: Option<String>,
}

pub struct App {
    pub focus: AppFocus,
    /// Pannello da ripristinare alla chiusura di un modale.
    pub base_focus: AppFocus,
    pub config: Config,
    pub profile_name: Option<String>,
    pub host: Option<String>,
    pub generation: u64,

    pub status: String,
    pub status_error: bool,
    pub pending: usize,
    pub messages_busy: bool,
    pub spinner: usize,

    pub topics: Vec<String>,
    pub topics_state: ListState,
    pub subs: Vec<SubscriptionInfo>,
    pub subs_state: ListState,
    pub messages: Vec<Message>,
    pub messages_state: ListState,
    /// (topic, sub, dlq) a cui si riferisce `messages`.
    pub messages_key: Option<(String, String, bool)>,
    pub dlq: bool,
    pub max_messages: usize,
    pub detail_scroll: u16,

    pub profiles_state: ListState,
    pub profile_form: Option<ProfileForm>,
    pub pending_action: Option<PendingAction>,

    pub hit: HitAreas,
    pub mouse_enabled: bool,
    pub should_quit: bool,
}

type Cmds = Vec<Command>;

impl App {
    pub fn new(config: Config) -> (Self, Cmds) {
        let mut app = Self {
            focus: AppFocus::TopicsList,
            base_focus: AppFocus::TopicsList,
            config,
            profile_name: None,
            host: None,
            generation: 0,
            status: "Ready".into(),
            status_error: false,
            pending: 0,
            messages_busy: false,
            spinner: 0,
            topics: vec![],
            topics_state: ListState::default(),
            subs: vec![],
            subs_state: ListState::default(),
            messages: vec![],
            messages_state: ListState::default(),
            messages_key: None,
            dlq: false,
            max_messages: DEFAULT_MAX_MESSAGES,
            detail_scroll: 0,
            profiles_state: ListState::default(),
            profile_form: None,
            pending_action: None,
            hit: HitAreas::default(),
            mouse_enabled: true,
            should_quit: false,
        };
        let mut cmds = vec![];
        match app.config.active().cloned() {
            Some(p) => {
                app.status = format!("Connessione a {}…", p.name);
                cmds.push(Command::Connect(p));
            }
            None => app.open_profiles(),
        }
        (app, cmds)
    }

    // ------------------------------------------------------------------ getters

    pub fn selected_topic(&self) -> Option<&String> {
        self.topics_state
            .selected()
            .and_then(|i| self.topics.get(i))
    }

    pub fn selected_sub(&self) -> Option<&SubscriptionInfo> {
        self.subs_state.selected().and_then(|i| self.subs.get(i))
    }

    pub fn selected_message(&self) -> Option<&Message> {
        self.messages_state
            .selected()
            .and_then(|i| self.messages.get(i))
    }

    pub fn is_busy(&self) -> bool {
        self.pending > 0 || self.messages_busy
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = one_line(&s.into());
        self.status_error = false;
    }

    fn set_error(&mut self, s: impl Into<String>) {
        self.status = one_line(&s.into());
        self.status_error = true;
    }

    fn request(&mut self, cmds: &mut Cmds, cmd: Command) {
        if matches!(cmd, Command::LoadTopics | Command::LoadSubscriptions { .. }) {
            self.pending += 1;
        }
        cmds.push(cmd);
    }

    // ------------------------------------------------------------------ comandi

    fn load_subscriptions(&mut self, cmds: &mut Cmds) {
        if let Some(topic) = self.selected_topic().cloned() {
            self.request(cmds, Command::LoadSubscriptions { topic });
        }
    }

    fn load_messages(&mut self, cmds: &mut Cmds) {
        if self.messages_busy {
            self.set_status("Operazione sui messaggi già in corso…");
            return;
        }
        let (Some(topic), Some(sub)) = (
            self.selected_topic().cloned(),
            self.selected_sub().map(|s| s.name.clone()),
        ) else {
            return;
        };
        self.messages_busy = true;
        self.set_status(format!(
            "Peek di {topic}/{sub}{}…",
            if self.dlq { " (DLQ)" } else { "" }
        ));
        cmds.push(Command::LoadMessages {
            topic,
            sub,
            dlq: self.dlq,
            max: self.max_messages,
        });
    }

    fn clear_messages(&mut self) {
        self.messages.clear();
        self.messages_state.select(None);
        self.messages_key = None;
        self.detail_scroll = 0;
    }

    fn open_profiles(&mut self) {
        if !matches!(
            self.focus,
            AppFocus::ProfilesModal | AppFocus::DeleteConfirmModal
        ) {
            self.base_focus = self.focus;
        }
        self.focus = AppFocus::ProfilesModal;
        let idx = self
            .profile_name
            .as_ref()
            .and_then(|n| self.config.profiles.iter().position(|p| &p.name == n))
            .or(if self.config.profiles.is_empty() {
                None
            } else {
                Some(0)
            });
        self.profiles_state.select(idx);
        if self.config.profiles.is_empty() {
            self.profile_form = Some(ProfileForm::default());
        }
    }

    fn activate_profile(&mut self, p: Profile, cmds: &mut Cmds) {
        self.config.active_profile = Some(p.name.clone());
        cmds.push(Command::SaveConfig(self.config.clone()));
        self.topics.clear();
        self.topics_state.select(None);
        self.subs.clear();
        self.subs_state.select(None);
        self.clear_messages();
        self.set_status(format!("Connessione a {}…", p.name));
        cmds.push(Command::Connect(p));
        self.focus = AppFocus::TopicsList;
    }

    // ------------------------------------------------------------------ eventi backend

    pub fn on_backend(&mut self, ev: Event) -> Cmds {
        let mut cmds = vec![];
        match ev {
            Event::Connected {
                generation,
                profile,
                host,
            } => {
                self.generation = generation;
                self.profile_name = Some(profile);
                self.host = Some(host.clone());
                self.pending = 0;
                self.messages_busy = false;
                self.set_status(format!("Connesso a {host}"));
                self.request(&mut cmds, Command::LoadTopics);
            }
            Event::Progress(s) => self.set_status(s),
            Event::Error { messages_op, msg } => {
                if messages_op {
                    self.messages_busy = false;
                } else {
                    self.pending = self.pending.saturating_sub(1);
                }
                self.set_error(msg);
            }
            Event::Topics { generation, topics } => {
                self.pending = self.pending.saturating_sub(1);
                if generation != self.generation {
                    return cmds;
                }
                let prev = self.selected_topic().cloned();
                self.topics = topics;
                let idx = prev
                    .and_then(|t| self.topics.iter().position(|x| *x == t))
                    .or(if self.topics.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                self.topics_state.select(idx);
                self.set_status(format!("{} topic", self.topics.len()));
                self.load_subscriptions(&mut cmds);
            }
            Event::Subscriptions {
                generation,
                topic,
                subs,
            } => {
                self.pending = self.pending.saturating_sub(1);
                if generation != self.generation || self.selected_topic() != Some(&topic) {
                    return cmds;
                }
                let prev = self.selected_sub().map(|s| s.name.clone());
                self.subs = subs;
                let idx = prev
                    .and_then(|n| self.subs.iter().position(|s| s.name == n))
                    .or(if self.subs.is_empty() { None } else { Some(0) });
                self.subs_state.select(idx);
                self.set_status(format!("{topic}: {} subscription", self.subs.len()));
            }
            Event::Messages {
                generation,
                topic,
                sub,
                dlq,
                messages,
            } => {
                self.messages_busy = false;
                if generation != self.generation || !self.is_current(&topic, &sub, dlq) {
                    return cmds;
                }
                self.set_status(format!(
                    "{} messaggi in {topic}/{sub}{}",
                    messages.len(),
                    if dlq { " (DLQ)" } else { "" }
                ));
                self.messages = messages;
                self.messages_state.select(if self.messages.is_empty() {
                    None
                } else {
                    Some(0)
                });
                self.messages_key = Some((topic, sub, dlq));
                self.detail_scroll = 0;
            }
            Event::MessageDeleted {
                generation,
                topic,
                sub,
                dlq,
                seq,
                found,
            } => {
                self.messages_busy = false;
                if generation != self.generation {
                    return cmds;
                }
                if found {
                    self.set_status(format!("Messaggio {seq} eliminato"));
                    if self.messages_key.as_ref() == Some(&(topic.clone(), sub.clone(), dlq))
                        && let Some(i) = self.messages.iter().position(|m| m.sequence_number == seq)
                    {
                        self.messages.remove(i);
                        let sel = if self.messages.is_empty() {
                            None
                        } else {
                            Some(i.min(self.messages.len() - 1))
                        };
                        self.messages_state.select(sel);
                        self.detail_scroll = 0;
                    }
                } else {
                    self.set_error(format!(
                        "Messaggio {seq} non trovato (già consumato o bloccato?)"
                    ));
                }
                if self.selected_topic() == Some(&topic) {
                    self.load_subscriptions(&mut cmds);
                }
            }
            Event::Purged {
                generation,
                topic,
                sub,
                dlq,
                count,
            } => {
                self.messages_busy = false;
                if generation != self.generation {
                    return cmds;
                }
                self.set_status(format!("Purge completato: {count} messaggi eliminati"));
                if self.messages_key.as_ref() == Some(&(topic.clone(), sub, dlq)) {
                    self.clear_messages();
                }
                if self.selected_topic() == Some(&topic) {
                    self.load_subscriptions(&mut cmds);
                }
            }
        }
        cmds
    }

    fn is_current(&self, topic: &str, sub: &str, dlq: bool) -> bool {
        self.selected_topic().map(String::as_str) == Some(topic)
            && self.selected_sub().map(|s| s.name.as_str()) == Some(sub)
            && self.dlq == dlq
    }

    // ------------------------------------------------------------------ input

    pub fn on_key(&mut self, key: KeyEvent) -> Cmds {
        let mut cmds = vec![];
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return cmds;
        }
        match self.focus {
            AppFocus::ProfilesModal => self.on_key_profiles(key, &mut cmds),
            AppFocus::DeleteConfirmModal => self.on_key_confirm(key, &mut cmds),
            _ => self.on_key_main(key, &mut cmds),
        }
        cmds
    }

    fn on_key_confirm(&mut self, key: KeyEvent, cmds: &mut Cmds) {
        let action = self.pending_action.take();
        self.focus = self.base_focus;
        if !matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.set_status("Operazione annullata");
            return;
        }
        if self.messages_busy {
            self.set_error("Operazione sui messaggi già in corso");
            return;
        }
        match action {
            Some(PendingAction::DeleteMessage {
                topic,
                sub,
                dlq,
                seq,
            }) => {
                self.messages_busy = true;
                self.set_status(format!("Eliminazione messaggio {seq}…"));
                cmds.push(Command::DeleteMessage {
                    topic,
                    sub,
                    dlq,
                    seq,
                });
            }
            Some(PendingAction::Purge { topic, sub, dlq }) => {
                self.messages_busy = true;
                self.set_status(format!("Purge di {topic}/{sub}…"));
                cmds.push(Command::Purge { topic, sub, dlq });
            }
            None => {}
        }
    }

    fn on_key_profiles(&mut self, key: KeyEvent, cmds: &mut Cmds) {
        if let Some(form) = self.profile_form.as_mut() {
            match key.code {
                KeyCode::Esc => self.profile_form = None,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    form.field = 1 - form.field
                }
                KeyCode::Backspace => {
                    let f = if form.field == 0 {
                        &mut form.name
                    } else {
                        &mut form.conn
                    };
                    f.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let f = if form.field == 0 {
                        &mut form.name
                    } else {
                        &mut form.conn
                    };
                    f.clear();
                }
                KeyCode::Char(c) => {
                    let f = if form.field == 0 {
                        &mut form.name
                    } else {
                        &mut form.conn
                    };
                    f.push(c);
                }
                KeyCode::Enter => {
                    if form.field == 0 {
                        form.field = 1;
                        return;
                    }
                    let name = form.name.trim().to_string();
                    let conn = form.conn.trim().to_string();
                    if name.is_empty() {
                        form.error = Some("Il nome è obbligatorio".into());
                        return;
                    }
                    if self.config.profiles.iter().any(|p| p.name == name) {
                        form.error = Some("Esiste già un profilo con questo nome".into());
                        return;
                    }
                    if let Err(e) = ConnectionInfo::parse(&conn) {
                        form.error = Some(format!("{e}"));
                        return;
                    }
                    let id = format!(
                        "{}-{}",
                        name.to_lowercase().replace(' ', "-"),
                        chrono::Utc::now().timestamp()
                    );
                    let p = Profile {
                        id,
                        name,
                        connection_string: conn,
                    };
                    self.config.profiles.push(p.clone());
                    self.profile_form = None;
                    self.activate_profile(p, cmds);
                }
                _ => {}
            }
            return;
        }

        let len = self.config.profiles.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => {
                if self.profile_name.is_some() {
                    self.focus = self.base_focus;
                } else if key.code == KeyCode::Char('q') {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => move_sel(&mut self.profiles_state, len, 1),
            KeyCode::Char('k') | KeyCode::Up => move_sel(&mut self.profiles_state, len, -1),
            KeyCode::Char('a') => self.profile_form = Some(ProfileForm::default()),
            KeyCode::Char('x') => {
                if let Some(i) = self.profiles_state.selected().filter(|&i| i < len) {
                    let removed = self.config.profiles.remove(i);
                    if self.config.active_profile.as_deref() == Some(removed.name.as_str()) {
                        self.config.active_profile = None;
                    }
                    let new_len = self.config.profiles.len();
                    self.profiles_state.select(if new_len == 0 {
                        None
                    } else {
                        Some(i.min(new_len - 1))
                    });
                    cmds.push(Command::SaveConfig(self.config.clone()));
                    self.set_status(format!("Profilo {} rimosso", removed.name));
                }
            }
            KeyCode::Enter => {
                if let Some(p) = self
                    .profiles_state
                    .selected()
                    .and_then(|i| self.config.profiles.get(i))
                    .cloned()
                {
                    self.activate_profile(p, cmds);
                }
            }
            _ => {}
        }
    }

    fn on_key_main(&mut self, key: KeyEvent, cmds: &mut Cmds) {
        use AppFocus::*;
        let delta: Option<isize> = match key.code {
            KeyCode::Char('j') | KeyCode::Down => Some(1),
            KeyCode::Char('k') | KeyCode::Up => Some(-1),
            KeyCode::PageDown => Some(10),
            KeyCode::PageUp => Some(-10),
            KeyCode::Char('g') | KeyCode::Home => Some(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => Some(isize::MAX / 2),
            _ => None,
        };
        if let Some(d) = delta {
            self.navigate(d, cmds);
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.cycle_focus(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.cycle_focus(-1),
            KeyCode::Esc => {
                if self.focus == MessageDetail {
                    self.focus = MessagesList;
                }
            }
            KeyCode::Char('p') => self.open_profiles(),
            KeyCode::Char('m') => {
                self.mouse_enabled = !self.mouse_enabled;
                self.set_status(if self.mouse_enabled {
                    "Mouse attivo"
                } else {
                    "Mouse disattivato: selezione del testo del terminale abilitata"
                });
            }
            KeyCode::Enter => match self.focus {
                TopicsList => self.focus = SubscriptionsList,
                SubscriptionsList => {
                    self.focus = MessagesList;
                    self.load_messages(cmds);
                }
                MessagesList if self.selected_message().is_some() => {
                    self.focus = MessageDetail;
                }
                _ => {}
            },
            KeyCode::Char('t') => {
                self.dlq = !self.dlq;
                self.clear_messages();
                if self.selected_sub().is_some()
                    && matches!(self.focus, MessagesList | MessageDetail)
                {
                    self.focus = MessagesList;
                    self.load_messages(cmds);
                } else {
                    self.set_status(format!(
                        "Vista: {}",
                        if self.dlq { "Dead-Letter" } else { "Active" }
                    ));
                }
            }
            KeyCode::Char('r') => match self.focus {
                TopicsList => self.request(cmds, Command::LoadTopics),
                SubscriptionsList => self.load_subscriptions(cmds),
                _ => {
                    self.load_messages(cmds);
                    self.load_subscriptions(cmds);
                }
            },
            KeyCode::Char('+') => {
                self.max_messages = (self.max_messages + 25).min(500);
                self.set_status(format!("Max messaggi per peek: {}", self.max_messages));
            }
            KeyCode::Char('-') => {
                self.max_messages = self.max_messages.saturating_sub(25).max(25);
                self.set_status(format!("Max messaggi per peek: {}", self.max_messages));
            }
            KeyCode::Char('d') | KeyCode::Delete
                if matches!(self.focus, MessagesList | MessageDetail) =>
            {
                if let (Some((topic, sub, dlq)), Some(m)) =
                    (self.messages_key.clone(), self.selected_message())
                {
                    self.pending_action = Some(PendingAction::DeleteMessage {
                        topic,
                        sub,
                        dlq,
                        seq: m.sequence_number,
                    });
                    self.base_focus = self.focus;
                    self.focus = DeleteConfirmModal;
                }
            }
            KeyCode::Char('P') => {
                if let (Some(topic), Some(sub)) = (
                    self.selected_topic().cloned(),
                    self.selected_sub().map(|s| s.name.clone()),
                ) {
                    self.pending_action = Some(PendingAction::Purge {
                        topic,
                        sub,
                        dlq: self.dlq,
                    });
                    self.base_focus = self.focus;
                    self.focus = DeleteConfirmModal;
                }
            }
            _ => {}
        }
    }

    fn cycle_focus(&mut self, dir: isize) {
        use AppFocus::*;
        let order = [TopicsList, SubscriptionsList, MessagesList, MessageDetail];
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = order[(i + dir).rem_euclid(order.len() as isize) as usize];
    }

    fn navigate(&mut self, d: isize, cmds: &mut Cmds) {
        self.navigate_panel(self.focus, d, cmds);
    }

    fn list_info(&self, panel: AppFocus) -> Option<(&ListState, usize)> {
        match panel {
            AppFocus::TopicsList => Some((&self.topics_state, self.topics.len())),
            AppFocus::SubscriptionsList => Some((&self.subs_state, self.subs.len())),
            AppFocus::MessagesList => Some((&self.messages_state, self.messages.len())),
            _ => None,
        }
    }

    fn navigate_panel(&mut self, panel: AppFocus, d: isize, cmds: &mut Cmds) {
        if panel == AppFocus::MessageDetail {
            let d = d.clamp(-10_000, 10_000);
            self.detail_scroll =
                (self.detail_scroll as isize + d).clamp(0, u16::MAX as isize) as u16;
            return;
        }
        let Some((state, len)) = self.list_info(panel) else {
            return;
        };
        if len == 0 {
            return;
        }
        let cur = state.selected().unwrap_or(0) as isize;
        let next = cur.saturating_add(d).clamp(0, len as isize - 1) as usize;
        self.select_in(panel, next, cmds);
    }

    /// Seleziona l'elemento `idx` di una lista applicando gli effetti collaterali
    /// (caricamento subscription, reset messaggi, ...). Usato da tastiera e mouse.
    fn select_in(&mut self, panel: AppFocus, idx: usize, cmds: &mut Cmds) {
        match panel {
            AppFocus::TopicsList if self.topics_state.selected() != Some(idx) => {
                self.topics_state.select(Some(idx));
                self.subs.clear();
                self.subs_state.select(None);
                self.clear_messages();
                self.load_subscriptions(cmds);
            }
            AppFocus::SubscriptionsList if self.subs_state.selected() != Some(idx) => {
                self.subs_state.select(Some(idx));
                // Nessun peek automatico: il peek-lock incrementa DeliveryCount.
                self.clear_messages();
            }
            AppFocus::MessagesList if self.messages_state.selected() != Some(idx) => {
                self.messages_state.select(Some(idx));
                self.detail_scroll = 0;
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ mouse

    pub fn on_mouse(&mut self, m: MouseEvent) -> Cmds {
        let mut cmds = vec![];
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.on_click(pos, &mut cmds),
            MouseEventKind::ScrollDown => self.on_scroll(pos, 1, &mut cmds),
            MouseEventKind::ScrollUp => self.on_scroll(pos, -1, &mut cmds),
            _ => {}
        }
        cmds
    }

    fn press(&mut self, code: KeyCode, cmds: &mut Cmds) {
        cmds.extend(self.on_key(KeyEvent::from(code)));
    }

    fn panel_at(&self, pos: Position) -> Option<(AppFocus, Rect)> {
        [
            (AppFocus::TopicsList, self.hit.topics),
            (AppFocus::SubscriptionsList, self.hit.subs),
            (AppFocus::MessagesList, self.hit.messages),
            (AppFocus::MessageDetail, self.hit.detail),
        ]
        .into_iter()
        .find(|(_, r)| r.contains(pos))
    }

    fn on_click(&mut self, pos: Position, cmds: &mut Cmds) {
        // I tasti nel footer sono cliccabili in qualunque modalità.
        if let Some(code) = self
            .hit
            .footer_keys
            .iter()
            .find(|(r, _)| r.contains(pos))
            .map(|(_, c)| *c)
        {
            self.press(code, cmds);
            return;
        }

        match self.focus {
            AppFocus::DeleteConfirmModal => {
                if self.hit.confirm_yes.contains(pos) {
                    self.press(KeyCode::Char('y'), cmds);
                } else if self.hit.confirm_no.contains(pos) || !self.hit.modal.contains(pos) {
                    self.press(KeyCode::Esc, cmds);
                }
            }
            AppFocus::ProfilesModal => {
                if !self.hit.modal.contains(pos) {
                    self.press(KeyCode::Esc, cmds);
                    return;
                }
                if let Some(form) = self.profile_form.as_mut() {
                    if let Some(i) = self.hit.form_fields.iter().position(|r| r.contains(pos)) {
                        form.field = i;
                    }
                    return;
                }
                let len = self.config.profiles.len();
                if let Some(i) =
                    list_index(self.hit.modal, &self.profiles_state, pos.y).filter(|&i| i < len)
                {
                    if self.profiles_state.selected() == Some(i) {
                        self.press(KeyCode::Enter, cmds);
                    } else {
                        self.profiles_state.select(Some(i));
                    }
                }
            }
            _ => {
                let Some((panel, area)) = self.panel_at(pos) else {
                    return;
                };
                if panel == AppFocus::MessageDetail {
                    if self.selected_message().is_some() {
                        self.focus = panel;
                    }
                    return;
                }
                self.focus = panel;
                let Some((state, len)) = self.list_info(panel) else {
                    return;
                };
                let selected = state.selected();
                let Some(idx) = list_index(area, state, pos.y).filter(|&i| i < len) else {
                    return;
                };
                if selected == Some(idx) {
                    // Click su elemento già selezionato = Enter.
                    self.press(KeyCode::Enter, cmds);
                } else {
                    self.select_in(panel, idx, cmds);
                }
            }
        }
    }

    fn on_scroll(&mut self, pos: Position, d: isize, cmds: &mut Cmds) {
        match self.focus {
            AppFocus::ProfilesModal if self.profile_form.is_none() => {
                move_sel(&mut self.profiles_state, self.config.profiles.len(), d);
            }
            AppFocus::ProfilesModal | AppFocus::DeleteConfirmModal => {}
            _ => {
                if let Some((panel, _)) = self.panel_at(pos) {
                    let step = if panel == AppFocus::MessageDetail {
                        3
                    } else {
                        1
                    };
                    self.navigate_panel(panel, d * step, cmds);
                }
            }
        }
    }
}

/// Indice dell'elemento di una lista con bordo alla riga `row`.
fn list_index(area: Rect, state: &ListState, row: u16) -> Option<usize> {
    if row <= area.y || row + 1 >= area.bottom() {
        return None;
    }
    Some(state.offset() + (row - area.y - 1) as usize)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn move_sel(state: &mut ListState, len: usize, d: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    let next = cur.saturating_add(d).clamp(0, len as isize - 1);
    state.select(Some(next as usize));
}
