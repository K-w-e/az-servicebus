use crate::azure::{
    ConnectionInfo, Message, SubscriptionInfo, SubscriptionOptions, validate_subscription_name,
};
use crate::config::{Config, Profile};
use crate::worker::{Command, Event};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

pub const DEFAULT_MAX_MESSAGES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppFocus {
    ProfilesModal,
    SubscriptionModal,
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
    DeleteSubscription {
        topic: String,
        sub: String,
    },
    Purge {
        topic: String,
        sub: String,
        dlq: bool,
    },
}

/// Clickable areas, updated by the UI on every render.
#[derive(Debug, Default, Clone)]
pub struct HitAreas {
    pub topics: Rect,
    pub subs: Rect,
    pub messages: Rect,
    pub detail: Rect,
    pub modal: Rect,
    pub form_fields: [Rect; 2],
    pub sub_form_fields: [Rect; 6],
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

pub const SUB_FORM_FIELD_COUNT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionFormMode {
    Create,
    Edit,
}

#[derive(Debug, Clone)]
pub struct SubscriptionForm {
    pub mode: SubscriptionFormMode,
    pub topic: String,
    pub name: String,
    pub max_delivery_count: String,
    pub lock_duration: String,
    pub default_message_ttl: String,
    pub dead_letter_on_expiration: bool,
    pub requires_session: bool,
    /// Original server-side options preserved when updating so untouched fields are not reset.
    pub base_options: SubscriptionOptions,
    pub field: usize,
    pub error: Option<String>,
}

impl SubscriptionForm {
    pub fn new_create(topic: String) -> Self {
        Self {
            mode: SubscriptionFormMode::Create,
            topic,
            name: String::new(),
            max_delivery_count: "10".into(),
            lock_duration: "PT1M".into(),
            default_message_ttl: "P14D".into(),
            dead_letter_on_expiration: false,
            requires_session: false,
            base_options: SubscriptionOptions::default(),
            field: 0,
            error: None,
        }
    }

    pub fn new_edit(topic: String, name: String, opts: SubscriptionOptions) -> Self {
        Self {
            mode: SubscriptionFormMode::Edit,
            topic,
            name,
            max_delivery_count: opts
                .max_delivery_count
                .map(|n| n.to_string())
                .unwrap_or_default(),
            lock_duration: opts.lock_duration.clone().unwrap_or_default(),
            default_message_ttl: opts.default_message_ttl.clone().unwrap_or_default(),
            dead_letter_on_expiration: opts.dead_lettering_on_message_expiration,
            requires_session: opts.requires_session,
            base_options: opts,
            field: 1,
            error: None,
        }
    }

    pub fn is_edit(&self) -> bool {
        self.mode == SubscriptionFormMode::Edit
    }

    pub fn is_field_editable(&self, field: usize) -> bool {
        if self.is_edit() {
            // Name (0) and RequiresSession (5) are immutable in Azure Service Bus.
            !matches!(field, 0 | 5)
        } else {
            field < SUB_FORM_FIELD_COUNT
        }
    }

    pub fn step_field(&mut self, dir: isize) {
        let count = SUB_FORM_FIELD_COUNT as isize;
        let mut next = self.field as isize;
        for _ in 0..SUB_FORM_FIELD_COUNT {
            next = (next + dir).rem_euclid(count);
            if self.is_field_editable(next as usize) {
                self.field = next as usize;
                return;
            }
        }
    }

    pub fn text_field_mut(&mut self) -> Option<&mut String> {
        if !self.is_field_editable(self.field) {
            return None;
        }
        match self.field {
            0 => Some(&mut self.name),
            1 => Some(&mut self.max_delivery_count),
            2 => Some(&mut self.lock_duration),
            3 => Some(&mut self.default_message_ttl),
            _ => None,
        }
    }

    pub fn toggle_bool_field(&mut self) -> bool {
        if !self.is_field_editable(self.field) {
            return false;
        }
        match self.field {
            4 => {
                self.dead_letter_on_expiration = !self.dead_letter_on_expiration;
                true
            }
            5 => {
                self.requires_session = !self.requires_session;
                true
            }
            _ => false,
        }
    }

    pub fn validate(&self) -> Result<(String, SubscriptionOptions), String> {
        let name = self.name.trim().to_string();
        if !self.is_edit() {
            validate_subscription_name(&name).map_err(|e| e.to_string())?;
        }

        let max_delivery_count = {
            let s = self.max_delivery_count.trim();
            if s.is_empty() {
                None
            } else {
                let n: u32 = s
                    .parse()
                    .map_err(|_| "Max delivery count must be a positive integer".to_string())?;
                if n == 0 {
                    return Err("Max delivery count must be at least 1".into());
                }
                Some(n)
            }
        };

        let lock_duration = parse_optional_iso_duration(&self.lock_duration, "Lock duration")?;
        let default_message_ttl =
            parse_optional_iso_duration(&self.default_message_ttl, "Default message TTL")?;

        let mut opts = self.base_options.clone();
        opts.lock_duration = lock_duration;
        opts.requires_session = self.requires_session;
        opts.default_message_ttl = default_message_ttl;
        opts.dead_lettering_on_message_expiration = self.dead_letter_on_expiration;
        opts.max_delivery_count = max_delivery_count;

        Ok((name, opts))
    }
}

fn parse_optional_iso_duration(raw: &str, label: &str) -> Result<Option<String>, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if !s.starts_with('P') || s.len() < 2 {
        return Err(format!(
            "{label} must be an ISO-8601 duration (e.g. PT1M, P14D)"
        ));
    }
    Ok(Some(s.to_string()))
}

pub struct App {
    pub focus: AppFocus,
    /// Panel to restore when closing a modal.
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
    pub sub_form: Option<SubscriptionForm>,
    /// Subscription name to highlight after reloading subscriptions (e.g. after creation).
    pub pending_select_sub: Option<String>,
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
            sub_form: None,
            pending_select_sub: None,
            pending_action: None,
            hit: HitAreas::default(),
            mouse_enabled: true,
            should_quit: false,
        };
        let mut cmds = vec![];
        match app.config.active().cloned() {
            Some(p) => {
                app.status = format!("Connecting to {}…", p.name);
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
        if matches!(
            cmd,
            Command::LoadTopics
                | Command::LoadSubscriptions { .. }
                | Command::LoadSubscriptionDetails { .. }
                | Command::CreateSubscription { .. }
                | Command::UpdateSubscription { .. }
                | Command::DeleteSubscription { .. }
        ) {
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
            self.set_status("A message operation is already in progress…");
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
            "Peeking at {topic}/{sub}{}…",
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
            AppFocus::ProfilesModal | AppFocus::SubscriptionModal | AppFocus::DeleteConfirmModal
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

    fn open_create_subscription(&mut self) {
        let Some(topic) = self.selected_topic().cloned() else {
            self.set_error("Select a topic first");
            return;
        };
        if !matches!(
            self.focus,
            AppFocus::ProfilesModal | AppFocus::SubscriptionModal | AppFocus::DeleteConfirmModal
        ) {
            self.base_focus = self.focus;
        }
        self.sub_form = Some(SubscriptionForm::new_create(topic));
        self.focus = AppFocus::SubscriptionModal;
    }

    fn open_edit_subscription(&mut self, cmds: &mut Cmds) {
        let (Some(topic), Some(sub)) = (
            self.selected_topic().cloned(),
            self.selected_sub().map(|s| s.name.clone()),
        ) else {
            self.set_error("Select a subscription first");
            return;
        };
        self.set_status(format!("Loading subscription '{sub}' properties…"));
        self.request(cmds, Command::LoadSubscriptionDetails { topic, name: sub });
    }

    fn activate_profile(&mut self, p: Profile, cmds: &mut Cmds) {
        self.config.active_profile = Some(p.name.clone());
        cmds.push(Command::SaveConfig(self.config.clone()));
        self.topics.clear();
        self.topics_state.select(None);
        self.subs.clear();
        self.subs_state.select(None);
        self.clear_messages();
        self.set_status(format!("Connecting to {}…", p.name));
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
                self.set_status(format!("Connected to {host}"));
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
                let target = self
                    .pending_select_sub
                    .take()
                    .or_else(|| self.selected_sub().map(|s| s.name.clone()));
                self.subs = subs;
                let idx = target
                    .and_then(|n| self.subs.iter().position(|s| s.name == n))
                    .or(if self.subs.is_empty() { None } else { Some(0) });
                self.subs_state.select(idx);
                self.set_status(format!("{topic}: {} subscription", self.subs.len()));
            }
            Event::SubscriptionDetails {
                generation,
                topic,
                name,
                options,
            } => {
                self.pending = self.pending.saturating_sub(1);
                if generation != self.generation
                    || self.selected_topic() != Some(&topic)
                    || self.selected_sub().map(|s| s.name.as_str()) != Some(name.as_str())
                {
                    return cmds;
                }
                if !matches!(
                    self.focus,
                    AppFocus::ProfilesModal
                        | AppFocus::SubscriptionModal
                        | AppFocus::DeleteConfirmModal
                ) {
                    self.base_focus = self.focus;
                }
                self.sub_form = Some(SubscriptionForm::new_edit(topic, name, options));
                self.focus = AppFocus::SubscriptionModal;
                self.set_status("Editing subscription");
            }
            Event::SubscriptionCreated {
                generation,
                topic,
                name,
            } => {
                self.pending = self.pending.saturating_sub(1);
                if generation != self.generation {
                    return cmds;
                }
                self.set_status(format!("Subscription '{name}' created in {topic}"));
                if self.selected_topic() == Some(&topic) {
                    self.pending_select_sub = Some(name);
                    self.clear_messages();
                    self.load_subscriptions(&mut cmds);
                }
            }
            Event::SubscriptionUpdated {
                generation,
                topic,
                name,
            } => {
                self.pending = self.pending.saturating_sub(1);
                if generation != self.generation {
                    return cmds;
                }
                self.set_status(format!("Subscription '{name}' updated in {topic}"));
                if self.selected_topic() == Some(&topic) {
                    self.pending_select_sub = Some(name);
                    self.load_subscriptions(&mut cmds);
                }
            }
            Event::SubscriptionDeleted {
                generation,
                topic,
                name,
            } => {
                self.pending = self.pending.saturating_sub(1);
                if generation != self.generation {
                    return cmds;
                }
                self.set_status(format!("Subscription '{name}' deleted from {topic}"));
                if self
                    .messages_key
                    .as_ref()
                    .is_some_and(|(t, s, _)| t == &topic && s == &name)
                {
                    self.clear_messages();
                }
                if self.selected_topic() == Some(&topic) {
                    if let Some(i) = self.subs.iter().position(|s| s.name == name) {
                        self.subs.remove(i);
                        let sel = if self.subs.is_empty() {
                            None
                        } else {
                            Some(i.min(self.subs.len() - 1))
                        };
                        self.subs_state.select(sel);
                    }
                    self.load_subscriptions(&mut cmds);
                }
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
                    "{} messages in {topic}/{sub}{}",
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
                    self.set_status(format!("Message {seq} deleted"));
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
                        "Message {seq} not found (already consumed or locked?)"
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
                self.set_status(format!("Purge complete: {count} messages deleted"));
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
            AppFocus::SubscriptionModal => self.on_key_sub_form(key, &mut cmds),
            AppFocus::DeleteConfirmModal => self.on_key_confirm(key, &mut cmds),
            _ => self.on_key_main(key, &mut cmds),
        }
        cmds
    }

    fn on_key_confirm(&mut self, key: KeyEvent, cmds: &mut Cmds) {
        let action = self.pending_action.take();
        self.focus = self.base_focus;
        if !matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.set_status("Operation cancelled");
            return;
        }
        match action {
            Some(PendingAction::DeleteSubscription { topic, sub }) => {
                self.set_status(format!("Deleting subscription '{sub}' from {topic}…"));
                self.request(cmds, Command::DeleteSubscription { topic, name: sub });
            }
            Some(PendingAction::DeleteMessage {
                topic,
                sub,
                dlq,
                seq,
            }) => {
                if self.messages_busy {
                    self.set_error("A message operation is already in progress");
                    return;
                }
                self.messages_busy = true;
                self.set_status(format!("Deleting message {seq}…"));
                cmds.push(Command::DeleteMessage {
                    topic,
                    sub,
                    dlq,
                    seq,
                });
            }
            Some(PendingAction::Purge { topic, sub, dlq }) => {
                if self.messages_busy {
                    self.set_error("A message operation is already in progress");
                    return;
                }
                self.messages_busy = true;
                self.set_status(format!("Purge di {topic}/{sub}…"));
                cmds.push(Command::Purge { topic, sub, dlq });
            }
            None => {}
        }
    }

    fn on_key_sub_form(&mut self, key: KeyEvent, cmds: &mut Cmds) {
        let Some(form) = self.sub_form.as_mut() else {
            self.focus = self.base_focus;
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.sub_form = None;
                self.focus = self.base_focus;
            }
            KeyCode::Tab | KeyCode::Down => form.step_field(1),
            KeyCode::BackTab | KeyCode::Up => form.step_field(-1),
            KeyCode::Char(' ') if form.toggle_bool_field() => {
                form.error = None;
            }
            KeyCode::Backspace => {
                if let Some(f) = form.text_field_mut() {
                    f.pop();
                    form.error = None;
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(f) = form.text_field_mut() {
                    f.clear();
                    form.error = None;
                }
            }
            KeyCode::Char(c) => {
                if let Some(f) = form.text_field_mut() {
                    f.push(c);
                    form.error = None;
                }
            }
            KeyCode::Enter => match form.validate() {
                Ok((name, options)) => {
                    let topic = form.topic.clone();
                    let is_edit = form.is_edit();
                    self.sub_form = None;
                    self.focus = AppFocus::SubscriptionsList;
                    if is_edit {
                        self.set_status(format!("Updating subscription '{name}' in {topic}…"));
                        self.request(
                            cmds,
                            Command::UpdateSubscription {
                                topic,
                                name,
                                options,
                            },
                        );
                    } else {
                        self.set_status(format!("Creating subscription '{name}' in {topic}…"));
                        self.request(
                            cmds,
                            Command::CreateSubscription {
                                topic,
                                name,
                                options,
                            },
                        );
                    }
                }
                Err(e) => {
                    form.error = Some(e);
                }
            },
            _ => {}
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
                        form.error = Some("Name is required".into());
                        return;
                    }
                    if self.config.profiles.iter().any(|p| p.name == name) {
                        form.error = Some("A profile with this name already exists".into());
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
                    self.set_status(format!("Profile {} removed", removed.name));
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
            KeyCode::Char('n') => self.open_create_subscription(),
            KeyCode::Char('e') => self.open_edit_subscription(cmds),
            KeyCode::Char('m') => {
                self.mouse_enabled = !self.mouse_enabled;
                self.set_status(if self.mouse_enabled {
                    "Mouse enabled"
                } else {
                    "Mouse disabled: terminal text selection enabled"
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
                        "View: {}",
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
                self.set_status(format!("Max messages per peek: {}", self.max_messages));
            }
            KeyCode::Char('-') => {
                self.max_messages = self.max_messages.saturating_sub(25).max(25);
                self.set_status(format!("Max messages per peek: {}", self.max_messages));
            }
            KeyCode::Char('d') | KeyCode::Delete => match self.focus {
                SubscriptionsList => {
                    if let (Some(topic), Some(sub)) = (
                        self.selected_topic().cloned(),
                        self.selected_sub().map(|s| s.name.clone()),
                    ) {
                        self.pending_action = Some(PendingAction::DeleteSubscription { topic, sub });
                        self.base_focus = self.focus;
                        self.focus = DeleteConfirmModal;
                    }
                }
                MessagesList | MessageDetail => {
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
                _ => {}
            },
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

    /// Select list item `idx` and apply side effects
    /// (loading subscriptions, resetting messages, ...). Used by keyboard and mouse.
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
                // No automatic peek: peek-lock increments DeliveryCount.
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
        // Footer keys are clickable in any mode.
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
            AppFocus::SubscriptionModal => {
                if !self.hit.modal.contains(pos) {
                    self.press(KeyCode::Esc, cmds);
                    return;
                }
                if let Some(form) = self.sub_form.as_mut()
                    && let Some(i) = self
                        .hit
                        .sub_form_fields
                        .iter()
                        .position(|r| r.contains(pos))
                    && form.is_field_editable(i)
                {
                    form.field = i;
                    form.toggle_bool_field();
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
                    // Clicking an already-selected item is equivalent to pressing Enter.
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
            AppFocus::ProfilesModal | AppFocus::SubscriptionModal | AppFocus::DeleteConfirmModal => {
            }
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

/// Index of the list item at `row`, accounting for the border.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app_with_topic() -> App {
        let (mut app, _) = App::new(Config::default());
        app.focus = AppFocus::TopicsList;
        app.profile_name = Some("test".into());
        app.generation = 1;
        app.topics = vec!["orders".into()];
        app.topics_state.select(Some(0));
        app
    }

    #[test]
    fn create_sub_form_validates_defaults_and_options() {
        let mut form = SubscriptionForm::new_create("orders".into());
        assert!(form.validate().is_err()); // empty name

        form.name = "billing-worker".into();
        let (name, opts) = form.validate().unwrap();
        assert_eq!(name, "billing-worker");
        assert_eq!(opts.max_delivery_count, Some(10));
        assert_eq!(opts.lock_duration.as_deref(), Some("PT1M"));
        assert_eq!(opts.default_message_ttl.as_deref(), Some("P14D"));
        assert!(!opts.dead_lettering_on_message_expiration);
        assert!(!opts.requires_session);

        form.max_delivery_count = "0".into();
        assert!(form.validate().is_err());

        form.max_delivery_count = "5".into();
        form.lock_duration = "60s".into();
        assert!(form.validate().is_err());
    }

    #[test]
    fn create_sub_modal_emits_command_and_selects_new_sub() {
        let mut app = test_app_with_topic();
        let cmds = app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(cmds.is_empty());
        assert_eq!(app.focus, AppFocus::SubscriptionModal);

        for c in "my-sub".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        // Navigate to DLQ-on-expiration field (index 4) and toggle it.
        for _ in 0..4 {
            app.on_key(KeyEvent::from(KeyCode::Tab));
        }
        app.on_key(KeyEvent::from(KeyCode::Char(' ')));

        let cmds = app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.focus, AppFocus::SubscriptionsList);
        assert!(app.sub_form.is_none());
        assert_eq!(app.pending, 1);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Command::CreateSubscription {
                topic,
                name,
                options,
            } => {
                assert_eq!(topic, "orders");
                assert_eq!(name, "my-sub");
                assert!(options.dead_lettering_on_message_expiration);
                assert!(!options.requires_session);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Backend confirms creation -> triggers subscriptions reload and remembers selection.
        let cmds = app.on_backend(Event::SubscriptionCreated {
            generation: 1,
            topic: "orders".into(),
            name: "my-sub".into(),
        });
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            Command::LoadSubscriptions { topic } if topic == "orders"
        ));

        app.on_backend(Event::Subscriptions {
            generation: 1,
            topic: "orders".into(),
            subs: vec![
                SubscriptionInfo {
                    name: "alpha".into(),
                    active_count: 0,
                    dead_letter_count: 0,
                },
                SubscriptionInfo {
                    name: "my-sub".into(),
                    active_count: 0,
                    dead_letter_count: 0,
                },
            ],
        });
        assert_eq!(app.selected_sub().map(|s| s.name.as_str()), Some("my-sub"));
    }

    #[test]
    fn edit_sub_modal_loads_preserves_extra_fields_and_skips_immutable() {
        let mut app = test_app_with_topic();
        app.subs = vec![SubscriptionInfo {
            name: "worker".into(),
            active_count: 3,
            dead_letter_count: 0,
        }];
        app.subs_state.select(Some(0));
        app.focus = AppFocus::SubscriptionsList;

        let cmds = app.on_key(KeyEvent::from(KeyCode::Char('e')));
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            Command::LoadSubscriptionDetails { topic, name } if topic == "orders" && name == "worker"
        ));

        let existing = SubscriptionOptions {
            lock_duration: Some("PT1M".into()),
            requires_session: true,
            default_message_ttl: Some("P7D".into()),
            dead_lettering_on_message_expiration: false,
            max_delivery_count: Some(10),
            forward_to: Some("other-queue".into()),
            user_metadata: Some("keep-me".into()),
            ..Default::default()
        };
        app.on_backend(Event::SubscriptionDetails {
            generation: 1,
            topic: "orders".into(),
            name: "worker".into(),
            options: existing,
        });
        assert_eq!(app.focus, AppFocus::SubscriptionModal);
        // Starts on field 1 (max_delivery_count) because name (0) is immutable.
        assert_eq!(app.sub_form.as_ref().unwrap().field, 1);

        // Replace max_delivery_count with 20.
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.on_key(KeyEvent::from(KeyCode::Char('0')));

        // Tab through fields 2, 3, 4, then wrap back to 1 (skipping 5 and 0).
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.sub_form.as_ref().unwrap().field, 2);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.sub_form.as_ref().unwrap().field, 3);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.sub_form.as_ref().unwrap().field, 4);
        app.on_key(KeyEvent::from(KeyCode::Char(' '))); // toggle DLQ on expiration -> true
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.sub_form.as_ref().unwrap().field, 1);

        let cmds = app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Command::UpdateSubscription {
                topic,
                name,
                options,
            } => {
                assert_eq!(topic, "orders");
                assert_eq!(name, "worker");
                assert_eq!(options.max_delivery_count, Some(20));
                assert!(options.dead_lettering_on_message_expiration);
                assert!(options.requires_session);
                assert_eq!(options.forward_to.as_deref(), Some("other-queue"));
                assert_eq!(options.user_metadata.as_deref(), Some("keep-me"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn delete_subscription_requires_confirm_and_clears_messages() {
        let mut app = test_app_with_topic();
        app.subs = vec![
            SubscriptionInfo {
                name: "temp-sub".into(),
                active_count: 1,
                dead_letter_count: 0,
            },
            SubscriptionInfo {
                name: "other".into(),
                active_count: 0,
                dead_letter_count: 0,
            },
        ];
        app.subs_state.select(Some(0));
        app.messages_key = Some(("orders".into(), "temp-sub".into(), false));
        app.focus = AppFocus::SubscriptionsList;

        let cmds = app.on_key(KeyEvent::from(KeyCode::Char('d')));
        assert!(cmds.is_empty());
        assert_eq!(app.focus, AppFocus::DeleteConfirmModal);
        assert!(matches!(
            &app.pending_action,
            Some(PendingAction::DeleteSubscription { topic, sub })
                if topic == "orders" && sub == "temp-sub"
        ));

        let cmds = app.on_key(KeyEvent::from(KeyCode::Char('y')));
        assert_eq!(app.focus, AppFocus::SubscriptionsList);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            Command::DeleteSubscription { topic, name }
                if topic == "orders" && name == "temp-sub"
        ));

        let cmds = app.on_backend(Event::SubscriptionDeleted {
            generation: 1,
            topic: "orders".into(),
            name: "temp-sub".into(),
        });
        assert!(app.messages_key.is_none());
        assert_eq!(app.selected_sub().map(|s| s.name.as_str()), Some("other"));
        assert!(matches!(
            cmds.as_slice(),
            [Command::LoadSubscriptions { topic }] if topic == "orders"
        ));
    }
}
