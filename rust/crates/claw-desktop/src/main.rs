//! claw desktop: a chat window for claw.
//!
//! Every chat runs a `claw desktop-agent` process in its working folder, so
//! the app uses the same settings, permissions, tools and sessions as claw in
//! the terminal. The sidebar lists chats started here and, on its own tab,
//! sessions started in the terminal, grouped under projects.

mod agent;
mod markdown_view;
mod sessions;
mod settings;
mod store;
mod theme;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use iced::keyboard;
use iced::widget::text::Wrapping;
use iced::widget::{
    button, checkbox, column, container, markdown, mouse_area, pick_list, rich_text, row, rule,
    scrollable, space, span, text, text_editor, text_input, toggler, tooltip, Column,
};
use iced::{Alignment, Element, Font, Length, Subscription, Task, Theme};
use serde_json::Value;

use agent::{Agent, AgentEvent};
use markdown_view::{HighlightCache, ImageState};
use sessions::{Entry, SessionSummary};
use store::{Project, ScheduledTask, Store};
use theme::sol;

const SIDEBAR_WIDTH: f32 = 300.0;
const MONO: Font = Font::MONOSPACE;
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

pub fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1240.0, 840.0),
            min_size: Some(iced::Size::new(640.0, 480.0)),
            icon: iced::window::icon::from_file_data(ICON_PNG, None).ok(),
            #[cfg(target_os = "linux")]
            platform_specific: iced::window::settings::PlatformSpecific {
                // Matches claw-desktop.desktop, and gives dwm a WM_CLASS.
                application_id: "claw-desktop".to_string(),
                ..Default::default()
            },
            ..iced::window::Settings::default()
        })
        .run()
}

/// A request from the agent waiting for the user.
#[derive(Debug, Clone)]
enum Pending {
    Permission {
        id: u64,
        tool: String,
        command: Option<String>,
        reason: Option<String>,
    },
    Question {
        id: u64,
        question: String,
        options: Vec<String>,
    },
}

// Items live in a Vec per chat; boxing the markdown would only add churn.
#[allow(clippy::large_enum_variant)]
enum Item {
    User(String),
    Assistant {
        content: markdown::Content,
        raw: String,
    },
    Thinking {
        text: String,
        open: bool,
    },
    Tool {
        name: String,
        input: String,
        output: Option<String>,
        is_error: bool,
        open: bool,
    },
    Notice(String),
    Error(String),
}

struct Chat {
    cwd: PathBuf,
    session_path: Option<PathBuf>,
    title: String,
    items: Vec<Item>,
    agent: Option<Agent>,
    busy: bool,
    /// The model chosen in the picker; `None` uses claw's default.
    model: Option<String>,
    /// The model the agent reported.
    active_model: Option<String>,
    pending: Option<Pending>,
    answer: String,
    context: Option<(u64, Option<u64>)>,
    /// Nothing is saved; the chat is gone when it is closed.
    incognito: bool,
    /// The user message being edited (its index in `items`): sending
    /// replaces it and everything after it.
    editing: Option<usize>,
    /// Typed prompts to keep when the agent next gets a prompt: set by an
    /// edit, sent as `rewind` (after the agent is started, if need be).
    rewind: Option<usize>,
    /// Background shells and monitors the agent has running: (id, command).
    background: Vec<(String, String)>,
    /// The user named the chat, so the first message does not, and the name
    /// is saved once the chat has a session file.
    custom_title: bool,
}

impl Chat {
    fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            session_path: None,
            title: "New chat".to_string(),
            items: Vec::new(),
            agent: None,
            busy: false,
            model: None,
            active_model: None,
            pending: None,
            answer: String::new(),
            context: None,
            incognito: false,
            editing: None,
            rewind: None,
            background: Vec::new(),
            custom_title: false,
        }
    }

    fn from_session(summary: &SessionSummary) -> Self {
        let mut chat = Self::new(summary.workspace.clone());
        chat.session_path = Some(summary.path.clone());
        chat.title.clone_from(&summary.title);
        chat.active_model.clone_from(&summary.model);
        chat.items = sessions::load_entries(&summary.path)
            .into_iter()
            .map(|entry| match entry {
                Entry::User(text) => Item::User(text),
                Entry::Assistant(text) => Item::Assistant {
                    content: markdown::Content::parse(&text),
                    raw: text,
                },
                Entry::Thinking(text) => Item::Thinking { text, open: false },
                Entry::Tool {
                    name,
                    input,
                    output,
                    is_error,
                } => Item::Tool {
                    name,
                    input,
                    output,
                    is_error,
                    open: false,
                },
                Entry::Notice(text) => Item::Notice(text),
            })
            .collect();
        chat
    }

    fn last_prompt(&self) -> Option<&str> {
        self.items.iter().rev().find_map(|item| match item {
            Item::User(text) => Some(text.as_str()),
            _ => None,
        })
    }
}

/// What fills the main area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Chat,
    ProjectForm,
    Settings,
}

/// The sidebar's list: chats started here, or sessions from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListTab {
    Chats,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Skills,
    Mcp,
    Tools,
    Schedule,
}

impl SettingsTab {
    const ALL: [Self; 5] = [
        Self::General,
        Self::Skills,
        Self::Mcp,
        Self::Tools,
        Self::Schedule,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Skills => "Skills",
            Self::Mcp => "MCP servers",
            Self::Tools => "Tools",
            Self::Schedule => "Scheduled tasks",
        }
    }
}

struct ProjectForm {
    /// Index of the project being edited; `None` for a new one.
    editing: Option<usize>,
    name: String,
    path: String,
    instructions: text_editor::Content,
    error: Option<String>,
}

impl ProjectForm {
    fn new() -> Self {
        Self {
            editing: None,
            name: String::new(),
            path: String::new(),
            instructions: text_editor::Content::new(),
            error: None,
        }
    }
}

/// The Settings screen's data and half-filled forms.
struct SettingsState {
    tab: SettingsTab,
    skills: Vec<settings::Skill>,
    skill_source: String,
    installing: bool,
    mcp: Vec<settings::McpServer>,
    mcp_name: String,
    mcp_target: String,
    mcp_env: text_editor::Content,
    denied: Vec<String>,
    task_name: String,
    task_schedule: String,
    task_folder: String,
    task_prompt: text_editor::Content,
    /// The last result: (ok, message).
    notice: Option<(bool, String)>,
    confirm_skill: Option<PathBuf>,
}

impl SettingsState {
    fn new() -> Self {
        Self {
            tab: SettingsTab::General,
            skills: Vec::new(),
            skill_source: String::new(),
            installing: false,
            mcp: Vec::new(),
            mcp_name: String::new(),
            mcp_target: String::new(),
            mcp_env: text_editor::Content::new(),
            denied: Vec::new(),
            task_name: String::new(),
            task_schedule: "0 9 * * *".to_string(),
            task_folder: String::new(),
            task_prompt: text_editor::Content::new(),
            notice: None,
            confirm_skill: None,
        }
    }

    fn reload(&mut self) {
        self.skills = settings::list_skills();
        self.mcp = settings::list_mcp();
        self.denied = settings::denied_tools();
    }
}

struct App {
    store: Store,
    sessions: Vec<SessionSummary>,
    search: String,
    screen: Screen,
    list_tab: ListTab,
    chats: HashMap<u64, Chat>,
    next_chat: u64,
    current: u64,
    composer: text_editor::Content,
    default_model: Option<String>,
    models: Vec<String>,
    confirm_delete: Option<PathBuf>,
    confirm_delete_project: Option<usize>,
    /// Multi-select mode for deleting several chats.
    selecting: bool,
    selected: BTreeSet<PathBuf>,
    confirm_bulk: bool,
    form: ProjectForm,
    settings: SettingsState,
    collapsed_projects: BTreeSet<PathBuf>,
    scanning: bool,
    notice: Option<String>,
    /// The new title being typed for the current chat.
    renaming: Option<String>,
    /// A sidebar chat being renamed: its session file and the draft name.
    renaming_session: Option<(PathBuf, String)>,
    images: HashMap<String, ImageState>,
    /// A message shown as plain, selectable text: (chat, item, its text).
    select: Option<(u64, usize, text_editor::Content)>,
    /// Images saved with their Save button: where to, or why not.
    saved: HashMap<String, Result<PathBuf, String>>,
    highlights: RefCell<HighlightCache>,
    focused: bool,
    started_ms: u64,
}

#[derive(Debug, Clone)]
enum Message {
    NewChat,
    NewIncognito,
    NewChatIn(PathBuf),
    OpenSession(PathBuf),
    AskDelete(PathBuf),
    ConfirmDelete,
    CancelDelete,
    ToggleSelecting,
    Select(PathBuf, bool),
    SelectAll,
    AskDeleteSelected,
    ConfirmDeleteSelected,
    ListTab(ListTab),
    ToggleSidebar,
    /// Ctrl+Q: end every chat's agent and close the app.
    Quit,
    Search(String),
    Composer(text_editor::Action),
    Send,
    Stop,
    Retry,
    Attach,
    Attached(Vec<PathBuf>),
    Agent(AgentEvent),
    PickFolder,
    FolderPicked(Option<PathBuf>),
    ModelSelected(String),
    Decide(&'static str),
    Answer(String),
    AnswerText(String),
    ToggleItem(usize),
    Link(markdown::Uri),
    Copy(String),
    ImageLoaded(String, ImageState),
    SaveImage(String),
    /// Shows a message as selectable text, or back as Markdown.
    SelectText(usize),
    /// Put this user message back in the composer to change and resend.
    EditPrompt(usize),
    CancelEdit,
    /// Stop one background job of the current chat, or all of them.
    StopBackground(Option<String>),
    SelectAction(text_editor::Action),
    ImageSaved(String, Result<PathBuf, String>),
    NewProject,
    EditProject(usize),
    AskDeleteProject(usize),
    ConfirmDeleteProject,
    ProjectName(String),
    ProjectPath(String),
    ProjectBrowse,
    ProjectFolderPicked(Option<PathBuf>),
    ProjectInstructions(text_editor::Action),
    SaveProject,
    CancelProject,
    ToggleProject(PathBuf),
    Refresh,
    Rescan,
    Scanned(BTreeSet<PathBuf>),
    ToggleTheme,
    StartRename,
    RenameText(String),
    FinishRename,
    /// Rename a chat from the sidebar, open or not.
    StartRenameSession(PathBuf),
    RenameSessionText(String),
    FinishRenameSession,
    /// Esc, or ✕ next to the name being edited.
    CancelRename,
    Focus(bool),
    Tick,
    // Settings
    OpenSettings,
    CloseSettings,
    SettingsTab(SettingsTab),
    DefaultFolderBrowse,
    DefaultFolderPicked(Option<PathBuf>),
    SetNotify(bool),
    SetShowThinking(bool),
    SetTextSize(u16),
    SkillSource(String),
    SkillBrowseFolder,
    SkillBrowseFile,
    SkillPicked(Option<PathBuf>),
    InstallSkill,
    SkillInstalled(Result<Vec<String>, String>),
    AskDeleteSkill(PathBuf),
    ConfirmDeleteSkill,
    OpenFolder(PathBuf),
    McpName(String),
    McpTarget(String),
    McpEnv(text_editor::Action),
    AddMcp,
    RemoveMcp(String),
    ToggleTool(String, bool),
    TaskName(String),
    TaskSchedule(String),
    TaskFolder(String),
    TaskBrowse,
    TaskFolderPicked(Option<PathBuf>),
    TaskPrompt(text_editor::Action),
    AddTask,
    DeleteTask(usize),
    ToggleTask(usize, bool),
    RunTask(usize),
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let store = Store::load();
        let (default_model, models) = store::model_choices();
        let cwd = store.new_chat_folder();
        let mut app = Self {
            sessions: Vec::new(),
            store,
            search: String::new(),
            screen: Screen::Chat,
            list_tab: ListTab::Chats,
            chats: HashMap::from([(0, Chat::new(cwd))]),
            next_chat: 1,
            current: 0,
            composer: text_editor::Content::new(),
            default_model,
            models,
            confirm_delete: None,
            confirm_delete_project: None,
            selecting: false,
            selected: BTreeSet::new(),
            confirm_bulk: false,
            form: ProjectForm::new(),
            settings: SettingsState::new(),
            collapsed_projects: BTreeSet::new(),
            scanning: false,
            notice: None,
            renaming: None,
            renaming_session: None,
            images: HashMap::new(),
            select: None,
            saved: HashMap::new(),
            highlights: RefCell::new(HighlightCache::default()),
            focused: true,
            started_ms: sessions::now_ms(),
        };
        app.refresh_sessions();
        // First start: look for sessions made in the terminal.
        let task = if app.store.scanned_home {
            Task::none()
        } else {
            app.scanning = true;
            scan_task()
        };
        (app, task)
    }

    fn title(&self) -> String {
        match self.chats.get(&self.current) {
            Some(chat) if chat.incognito => "Incognito — Claw".to_string(),
            Some(chat) if chat.session_path.is_some() => format!("{} — Claw", chat.title),
            _ => "Claw".to_string(),
        }
    }

    fn theme(&self) -> Theme {
        theme::app_theme(self.store.light_theme)
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run(agent::events).map(Message::Agent),
            // Sessions started or continued in the terminal show up.
            iced::time::every(Duration::from_secs(8)).map(|_| Message::Refresh),
            // Scheduled tasks.
            iced::time::every(Duration::from_secs(30)).map(|_| Message::Tick),
            iced::event::listen_with(|event, _status, _window| match event {
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                }) => Some(Message::CancelRename),
                iced::Event::Window(iced::window::Event::Focused) => Some(Message::Focus(true)),
                iced::Event::Window(iced::window::Event::Unfocused) => Some(Message::Focus(false)),
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(key),
                    modifiers,
                    ..
                }) if modifiers.control() => match (key.as_str(), modifiers.shift()) {
                    ("b", false) => Some(Message::ToggleSidebar),
                    ("n", false) => Some(Message::NewChat),
                    ("n" | "N", true) => Some(Message::NewIncognito),
                    (",", false) => Some(Message::OpenSettings),
                    ("q", false) => Some(Message::Quit),
                    _ => None,
                },
                _ => None,
            }),
        ])
    }

    fn chat(&mut self) -> &mut Chat {
        let current = self.current;
        let folder = self.store.new_chat_folder();
        self.chats
            .entry(current)
            .or_insert_with(|| Chat::new(folder))
    }

    fn refresh_sessions(&mut self) {
        self.sessions = sessions::list_sessions(&self.store.workspaces);
        for session in &mut self.sessions {
            if let Some(title) = self.store.titles.get(&session.path) {
                session.title.clone_from(title);
            }
        }
        let known: BTreeSet<&PathBuf> = self.sessions.iter().map(|s| &s.path).collect();
        self.selected.retain(|path| known.contains(path));
    }

    fn is_app_chat(&self, session: &SessionSummary) -> bool {
        self.store.app_sessions.contains(&session.path)
    }

    fn open_new_chat(&mut self, cwd: PathBuf, incognito: bool) {
        // Reuse an untouched new chat instead of piling them up.
        let reusable = self.chats.iter().find_map(|(key, chat)| {
            (chat.session_path.is_none() && chat.items.is_empty() && chat.agent.is_none())
                .then_some(*key)
        });
        let key = reusable.unwrap_or_else(|| {
            let key = self.next_chat;
            self.next_chat += 1;
            key
        });
        let mut chat = Chat::new(cwd);
        chat.incognito = incognito;
        if incognito {
            chat.title = "Incognito chat".to_string();
        }
        self.chats.insert(key, chat);
        self.current = key;
        self.screen = Screen::Chat;
        self.composer = text_editor::Content::new();
    }

    /// Closes open chats on these sessions and deletes the files.
    fn delete_sessions(&mut self, paths: &[PathBuf]) {
        let mut failed = 0;
        for path in paths {
            // Close the chat first so its agent does not save it again.
            let open: Vec<u64> = self
                .chats
                .iter()
                .filter_map(|(key, chat)| {
                    (chat.session_path.as_ref() == Some(path)).then_some(*key)
                })
                .collect();
            for key in open {
                let cwd = self.chats.remove(&key).map(|chat| chat.cwd);
                if key == self.current {
                    self.open_new_chat(cwd.unwrap_or_else(|| self.store.new_chat_folder()), false);
                }
            }
            self.store.titles.remove(path);
            self.store.app_sessions.remove(path);
            self.selected.remove(path);
            if sessions::delete_session(path).is_err() {
                failed += 1;
            }
        }
        self.store.save();
        self.refresh_sessions();
        self.notice = Some(match (paths.len(), failed) {
            (_, 0) if paths.len() == 1 => "Deleted 1 chat.".to_string(),
            (count, 0) => format!("Deleted {count} chats."),
            (_, failed) => format!("Could not delete {failed} chat file(s)."),
        });
    }

    /// Starts loading the images that the chat's replies show.
    fn load_images(&mut self, key: u64) -> Task<Message> {
        let Some(chat) = self.chats.get(&key) else {
            return Task::none();
        };
        let mut wanted: Vec<String> = Vec::new();
        for item in &chat.items {
            if let Item::Assistant { content, .. } = item {
                for url in markdown_view::wanted_images(content, &self.images) {
                    if !wanted.contains(url) {
                        wanted.push(url.clone());
                    }
                }
            }
        }
        let mut tasks = Vec::new();
        for url in wanted {
            self.images.insert(url.clone(), ImageState::Loading);
            tasks.push(Task::perform(
                markdown_view::load_image(url),
                |(url, state)| Message::ImageLoaded(url, state),
            ));
        }
        Task::batch(tasks)
    }

    #[allow(clippy::too_many_lines)]
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::NewChat => {
                let folder = self.store.new_chat_folder();
                self.open_new_chat(folder, false);
            }
            Message::NewIncognito => {
                let folder = self.store.new_chat_folder();
                self.open_new_chat(folder, true);
            }
            Message::NewChatIn(cwd) => self.open_new_chat(cwd, false),
            Message::OpenSession(path) => {
                if self.selecting {
                    let selected = !self.selected.contains(&path);
                    return self.update(Message::Select(path, selected));
                }
                self.screen = Screen::Chat;
                if let Some(key) = self.chats.iter().find_map(|(key, chat)| {
                    (chat.session_path.as_ref() == Some(&path)).then_some(*key)
                }) {
                    self.current = key;
                } else if let Some(summary) = self.sessions.iter().find(|s| s.path == path) {
                    let key = self.next_chat;
                    self.next_chat += 1;
                    let mut chat = Chat::from_session(summary);
                    if self.store.show_thinking {
                        for item in &mut chat.items {
                            if let Item::Thinking { open, .. } = item {
                                *open = true;
                            }
                        }
                    }
                    self.chats.insert(key, chat);
                    self.current = key;
                    self.composer = text_editor::Content::new();
                    return self.load_images(key);
                }
                self.composer = text_editor::Content::new();
            }
            Message::AskDelete(path) => self.confirm_delete = Some(path),
            Message::CancelDelete => {
                self.confirm_delete = None;
                self.confirm_delete_project = None;
                self.confirm_bulk = false;
                self.settings.confirm_skill = None;
            }
            Message::ConfirmDelete => {
                if let Some(path) = self.confirm_delete.take() {
                    self.delete_sessions(&[path]);
                }
            }
            Message::ToggleSelecting => {
                self.selecting = !self.selecting;
                self.selected.clear();
                self.confirm_bulk = false;
            }
            Message::Select(path, on) => {
                if on {
                    self.selected.insert(path);
                } else {
                    self.selected.remove(&path);
                }
                self.confirm_bulk = false;
            }
            Message::SelectAll => {
                let visible: Vec<PathBuf> = self
                    .visible_sessions()
                    .into_iter()
                    .map(|session| session.path.clone())
                    .collect();
                if visible.iter().all(|path| self.selected.contains(path)) {
                    self.selected.clear();
                } else {
                    self.selected.extend(visible);
                }
            }
            Message::AskDeleteSelected => self.confirm_bulk = !self.selected.is_empty(),
            Message::ConfirmDeleteSelected => {
                let paths: Vec<PathBuf> = self.selected.iter().cloned().collect();
                self.delete_sessions(&paths);
                self.confirm_bulk = false;
                self.selecting = false;
            }
            Message::ListTab(tab) => {
                self.list_tab = tab;
                self.selected.clear();
                self.confirm_bulk = false;
            }
            Message::ToggleSidebar => {
                self.store.sidebar_hidden = !self.store.sidebar_hidden;
                self.store.save();
            }
            Message::Quit => {
                // Ask every agent to finish at once, then wait for them as
                // they are dropped.
                for chat in self.chats.values_mut() {
                    if let Some(agent) = chat.agent.as_mut() {
                        agent.shutdown();
                    }
                }
                self.chats.clear();
                self.store.save();
                return iced::exit();
            }
            Message::Search(query) => self.search = query,
            Message::Composer(action) => self.composer.perform(action),
            Message::Send => {
                let text = self.composer.text().trim().to_string();
                let chat = self.chat();
                if let (Some(index), false) = (chat.editing, chat.busy) {
                    if !text.is_empty() {
                        // The edited prompt replaces that message and the rest.
                        let kept = chat.items[..index.min(chat.items.len())]
                            .iter()
                            .filter(|item| matches!(item, Item::User(_)))
                            .count();
                        chat.items.truncate(index);
                        chat.rewind = Some(kept);
                        chat.editing = None;
                        self.select = None;
                    }
                }
                if !text.is_empty() && self.start_turn(self.current, &text) {
                    self.composer = text_editor::Content::new();
                }
            }
            Message::EditPrompt(index) => {
                let chat = self.chat();
                if let (Some(Item::User(prompt)), false) = (chat.items.get(index), chat.busy) {
                    let prompt = prompt.clone();
                    chat.editing = Some(index);
                    self.composer = text_editor::Content::with_text(&prompt);
                    self.composer
                        .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
                }
            }
            Message::StopBackground(id) => {
                if let Some(agent) = self.chat().agent.as_mut() {
                    agent.stop_background(id.as_deref());
                }
            }
            Message::CancelEdit => {
                self.chat().editing = None;
                self.composer = text_editor::Content::new();
            }
            Message::Retry => {
                if let Some(text) = self.chat().last_prompt().map(str::to_string) {
                    self.start_turn(self.current, &text);
                }
            }
            Message::Stop => {
                if let Some(agent) = self.chat().agent.as_mut() {
                    agent.cancel();
                }
            }
            Message::Attach => {
                let start = self.chat().cwd.clone();
                return Task::perform(pick_files(start), Message::Attached);
            }
            Message::Attached(paths) => {
                if !paths.is_empty() {
                    let mut text = self.composer.text().trim_end().to_string();
                    for path in paths {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!("[Attached file: {}]", path.display()));
                    }
                    text.push('\n');
                    self.composer = text_editor::Content::with_text(&text);
                    self.composer
                        .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
                }
            }
            Message::Agent(event) => return self.on_agent_event(event),
            Message::PickFolder => {
                let start = self.chat().cwd.clone();
                return Task::perform(pick_folder(start), Message::FolderPicked);
            }
            Message::FolderPicked(Some(folder)) => {
                let chat = self.chat();
                if chat.session_path.is_none() && chat.agent.is_none() {
                    chat.cwd.clone_from(&folder);
                }
                self.store.last_cwd = Some(folder);
                self.store.save();
            }
            Message::FolderPicked(None)
            | Message::ProjectFolderPicked(None)
            | Message::DefaultFolderPicked(None)
            | Message::SkillPicked(None)
            | Message::TaskFolderPicked(None) => {}
            Message::ModelSelected(choice) => {
                let model = store::model_id(&choice);
                let chat = self.chat();
                if let Some(agent) = chat.agent.as_mut() {
                    agent.set_model(&model);
                }
                chat.model = Some(model);
            }
            Message::Decide(decision) => {
                let chat = self.chat();
                if let (Some(Pending::Permission { id, .. }), Some(agent)) =
                    (chat.pending.take(), chat.agent.as_mut())
                {
                    agent.permission(id, decision);
                }
            }
            Message::Answer(text) => {
                let chat = self.chat();
                if let (Some(Pending::Question { id, .. }), Some(agent)) =
                    (chat.pending.take(), chat.agent.as_mut())
                {
                    agent.answer(id, &text);
                    chat.answer.clear();
                }
            }
            Message::AnswerText(text) => self.chat().answer = text,
            Message::ToggleItem(index) => {
                if let Some(Item::Tool { open, .. } | Item::Thinking { open, .. }) =
                    self.chat().items.get_mut(index)
                {
                    *open = !*open;
                }
            }
            Message::Link(url) => {
                let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            }
            Message::OpenFolder(path) => {
                let _ = std::process::Command::new("xdg-open").arg(path).spawn();
            }
            Message::Copy(text) => return iced::clipboard::write(text),
            Message::ImageLoaded(url, state) => {
                self.images.insert(url, state);
            }
            Message::SelectText(index) => {
                let current = self.current;
                if self
                    .select
                    .as_ref()
                    .is_some_and(|(chat, item, _)| *chat == current && *item == index)
                {
                    self.select = None;
                } else {
                    let raw = match self.chat().items.get(index) {
                        Some(Item::Assistant { raw, .. }) => Some(raw.clone()),
                        Some(Item::User(text)) => Some(text.clone()),
                        _ => None,
                    };
                    self.select =
                        raw.map(|raw| (current, index, text_editor::Content::with_text(&raw)));
                }
            }
            Message::SelectAction(action) => {
                // Read-only: selecting, moving and copying, never editing.
                if !action.is_edit() {
                    if let Some((_, _, content)) = self.select.as_mut() {
                        content.perform(action);
                    }
                }
            }
            Message::SaveImage(url) => {
                let key = url.clone();
                return Task::perform(markdown_view::save_image(url), move |result| {
                    Message::ImageSaved(key.clone(), result)
                });
            }
            Message::ImageSaved(url, result) => {
                self.notice = Some(match &result {
                    Ok(path) => format!("Saved {}", sessions::short_path(path)),
                    Err(error) => format!("Image not saved: {error}"),
                });
                self.saved.insert(url, result);
            }
            Message::NewProject => {
                self.form = ProjectForm::new();
                self.form.path = sessions::short_path(&self.chat().cwd.clone());
                self.screen = Screen::ProjectForm;
            }
            Message::EditProject(index) => {
                if let Some(project) = self.store.projects.get(index) {
                    self.form = ProjectForm {
                        editing: Some(index),
                        name: project.name.clone(),
                        path: project.path.display().to_string(),
                        instructions: text_editor::Content::with_text(&store::read_instructions(
                            &project.path,
                        )),
                        error: None,
                    };
                    self.screen = Screen::ProjectForm;
                }
            }
            Message::AskDeleteProject(index) => self.confirm_delete_project = Some(index),
            Message::ConfirmDeleteProject => {
                if let Some(index) = self.confirm_delete_project.take() {
                    if index < self.store.projects.len() {
                        // The folder, its files and its sessions stay.
                        self.store.projects.remove(index);
                        self.store.save();
                    }
                    self.screen = Screen::Chat;
                }
            }
            Message::ProjectName(name) => self.form.name = name,
            Message::ProjectPath(path) => self.form.path = path,
            Message::ProjectBrowse => {
                let start = expand_path(&self.form.path);
                return Task::perform(pick_folder(start), Message::ProjectFolderPicked);
            }
            Message::ProjectFolderPicked(Some(folder)) => {
                if self.form.name.trim().is_empty() {
                    if let Some(name) = folder.file_name() {
                        self.form.name = name.to_string_lossy().into_owned();
                    }
                }
                self.form.path = folder.display().to_string();
            }
            Message::ProjectInstructions(action) => self.form.instructions.perform(action),
            Message::SaveProject => return self.save_project(),
            Message::CancelProject => self.screen = Screen::Chat,
            Message::ToggleProject(path) => {
                if !self.collapsed_projects.remove(&path) {
                    self.collapsed_projects.insert(path);
                }
            }
            Message::Refresh => {
                self.refresh_sessions();
                let (default_model, models) = store::model_choices();
                self.default_model = default_model;
                self.models = models;
            }
            Message::Rescan => {
                if !self.scanning {
                    self.scanning = true;
                    return scan_task();
                }
            }
            Message::Scanned(found) => {
                self.scanning = false;
                let before = self.store.workspaces.len();
                self.store.workspaces.extend(found);
                self.store.scanned_home = true;
                self.store.save();
                self.refresh_sessions();
                let added = self.store.workspaces.len() - before;
                self.notice = Some(if added == 0 {
                    "No new session folders found.".to_string()
                } else {
                    format!("Found sessions in {added} more folder(s).")
                });
            }
            Message::StartRename => {
                let title = self.chat().title.clone();
                self.renaming = Some(title);
                return iced::widget::operation::focus("rename");
            }
            Message::RenameText(title) => self.renaming = Some(title),
            Message::FinishRename => {
                if let Some(title) = self.renaming.take() {
                    let title = title.trim().to_string();
                    let chat = self.chat();
                    if !title.is_empty() {
                        chat.title.clone_from(&title);
                        chat.custom_title = true;
                        // A new chat keeps the name until it is saved ("ready").
                        if let Some(path) = chat.session_path.clone() {
                            self.save_title(path, title);
                        }
                    }
                }
            }
            Message::StartRenameSession(path) => {
                let current = self
                    .sessions
                    .iter()
                    .find(|session| session.path == path)
                    .map(|session| session.title.clone())
                    .unwrap_or_default();
                self.renaming_session = Some((path, current));
                return iced::widget::operation::focus("rename-session");
            }
            Message::RenameSessionText(draft) => {
                if let Some((_, title)) = self.renaming_session.as_mut() {
                    *title = draft;
                }
            }
            Message::FinishRenameSession => {
                if let Some((path, title)) = self.renaming_session.take() {
                    let title = title.trim().to_string();
                    if !title.is_empty() {
                        for chat in self.chats.values_mut() {
                            if chat.session_path.as_ref() == Some(&path) {
                                chat.title.clone_from(&title);
                                chat.custom_title = true;
                            }
                        }
                        self.save_title(path, title);
                    }
                }
            }
            Message::CancelRename => {
                self.renaming = None;
                self.renaming_session = None;
            }
            Message::ToggleTheme => {
                self.store.light_theme = !self.store.light_theme;
                self.store.save();
            }
            Message::Focus(focused) => self.focused = focused,
            Message::Tick => return self.run_due_tasks(),
            Message::OpenSettings => {
                self.settings.reload();
                self.settings.notice = None;
                self.screen = Screen::Settings;
            }
            Message::CloseSettings => self.screen = Screen::Chat,
            Message::SettingsTab(tab) => {
                self.settings.reload();
                self.settings.tab = tab;
                self.settings.notice = None;
            }
            message => return self.update_settings(message),
        }
        Task::none()
    }

    /// The Settings screen's messages.
    fn update_settings(&mut self, message: Message) -> Task<Message> {
        let state = &mut self.settings;
        match message {
            Message::DefaultFolderBrowse => {
                let start = self.store.new_chat_folder();
                return Task::perform(pick_folder(start), Message::DefaultFolderPicked);
            }
            Message::DefaultFolderPicked(Some(folder)) => {
                self.store.default_cwd = Some(folder.clone());
                self.store.save();
                // An untouched new chat moves there too.
                let chat = self.chat();
                if chat.items.is_empty() && chat.agent.is_none() && chat.session_path.is_none() {
                    chat.cwd = folder;
                }
            }
            Message::SetNotify(on) => {
                self.store.notify_done = on;
                self.store.save();
            }
            Message::SetShowThinking(on) => {
                self.store.show_thinking = on;
                self.store.save();
            }
            Message::SetTextSize(size) => {
                self.store.text_size = size.clamp(12, 20);
                self.store.save();
            }
            Message::SkillSource(source) => state.skill_source = source,
            Message::SkillBrowseFolder => {
                return Task::perform(pick_folder(sessions::home_dir()), Message::SkillPicked);
            }
            Message::SkillBrowseFile => {
                return Task::perform(pick_skill_file(), Message::SkillPicked);
            }
            Message::SkillPicked(Some(path)) => state.skill_source = path.display().to_string(),
            Message::InstallSkill => {
                if state.skill_source.trim().is_empty() || state.installing {
                    return Task::none();
                }
                state.installing = true;
                let source = state.skill_source.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || settings::install_skill(&source))
                            .await
                            .unwrap_or_else(|error| Err(error.to_string()))
                    },
                    Message::SkillInstalled,
                );
            }
            Message::SkillInstalled(result) => {
                state.installing = false;
                state.notice = Some(match result {
                    Ok(names) => {
                        state.skill_source.clear();
                        (true, format!("Installed: {}", names.join(", ")))
                    }
                    Err(error) => (false, format!("Could not install: {error}")),
                });
                state.reload();
            }
            Message::AskDeleteSkill(dir) => state.confirm_skill = Some(dir),
            Message::ConfirmDeleteSkill => {
                if let Some(dir) = state.confirm_skill.take() {
                    state.notice = Some(match settings::delete_skill(&dir) {
                        Ok(()) => (true, format!("Removed {}", sessions::short_path(&dir))),
                        Err(error) => (false, format!("Could not remove it: {error}")),
                    });
                    state.reload();
                }
            }
            Message::McpName(name) => state.mcp_name = name,
            Message::McpTarget(target) => state.mcp_target = target,
            Message::McpEnv(action) => state.mcp_env.perform(action),
            Message::AddMcp => {
                let name = state.mcp_name.trim().to_string();
                state.notice = Some(
                    match settings::add_mcp(&name, &state.mcp_target, &state.mcp_env.text()) {
                        Ok(()) => {
                            state.mcp_name.clear();
                            state.mcp_target.clear();
                            state.mcp_env = text_editor::Content::new();
                            (
                                true,
                                format!("Added {name}. New chats connect to it; open chats keep their tools."),
                            )
                        }
                        Err(error) => (false, error),
                    },
                );
                state.reload();
            }
            Message::RemoveMcp(name) => {
                state.notice = Some(match settings::remove_mcp(&name) {
                    Ok(()) => (true, format!("Removed {name}.")),
                    Err(error) => (false, error),
                });
                state.reload();
            }
            Message::ToggleTool(tool, enabled) => {
                if let Err(error) = settings::set_tool_enabled(&tool, enabled) {
                    state.notice = Some((false, error));
                }
                state.reload();
            }
            Message::TaskName(name) => state.task_name = name,
            Message::TaskSchedule(schedule) => state.task_schedule = schedule,
            Message::TaskFolder(folder) => state.task_folder = folder,
            Message::TaskBrowse => {
                let start = self.store.new_chat_folder();
                return Task::perform(pick_folder(start), Message::TaskFolderPicked);
            }
            Message::TaskFolderPicked(Some(folder)) => {
                state.task_folder = folder.display().to_string();
            }
            Message::TaskPrompt(action) => state.task_prompt.perform(action),
            Message::AddTask => {
                let prompt = state.task_prompt.text().trim().to_string();
                let name = state.task_name.trim().to_string();
                let schedule = state.task_schedule.trim().to_string();
                let error = if name.is_empty() {
                    Some("Give the task a name.".to_string())
                } else if prompt.is_empty() {
                    Some("Write what claw should do.".to_string())
                } else {
                    runtime::scheduler::CronSchedule::parse(&schedule).err()
                };
                if let Some(error) = error {
                    state.notice = Some((false, error));
                    return Task::none();
                }
                let folder = if state.task_folder.trim().is_empty() {
                    self.store.new_chat_folder()
                } else {
                    expand_path(state.task_folder.trim())
                };
                self.store.tasks.push(ScheduledTask {
                    name: name.clone(),
                    schedule,
                    prompt,
                    folder,
                    enabled: true,
                    last_run_ms: Some(sessions::now_ms()),
                });
                self.store.save();
                state.task_name.clear();
                state.task_prompt = text_editor::Content::new();
                state.notice = Some((true, format!("Scheduled \"{name}\".")));
            }
            Message::DeleteTask(index) if index < self.store.tasks.len() => {
                self.store.tasks.remove(index);
                self.store.save();
            }
            Message::ToggleTask(index, on) => {
                if let Some(task) = self.store.tasks.get_mut(index) {
                    task.enabled = on;
                    task.last_run_ms = Some(sessions::now_ms());
                    self.store.save();
                }
            }
            Message::RunTask(index) => {
                if let Some(task) = self.store.tasks.get(index).cloned() {
                    self.run_task(index, &task);
                }
            }
            _ => {}
        }
        Task::none()
    }

    /// Sends `text` in a chat, starting its agent first if needed.
    fn start_turn(&mut self, key: u64, text: &str) -> bool {
        let Some(chat) = self.chats.get_mut(&key) else {
            return false;
        };
        if chat.busy {
            return false;
        }
        if chat.agent.is_none() {
            match Agent::spawn(
                key,
                &chat.cwd,
                chat.session_path.as_deref(),
                chat.model.as_deref(),
                chat.incognito,
            ) {
                Ok(agent) => chat.agent = Some(agent),
                Err(error) => {
                    chat.items
                        .push(Item::Error(format!("Could not start claw: {error}")));
                    return false;
                }
            }
        }
        if chat.session_path.is_none()
            && chat.items.is_empty()
            && !chat.incognito
            && !chat.custom_title
        {
            chat.title = sessions::clean_prompt(text)
                .split_whitespace()
                .take(10)
                .collect::<Vec<_>>()
                .join(" ");
        }
        chat.items.push(Item::User(text.to_string()));
        chat.busy = true;
        if let Some(agent) = chat.agent.as_mut() {
            if let Some(kept) = chat.rewind.take() {
                agent.rewind(kept);
            }
            agent.prompt(text);
        }
        let cwd = chat.cwd.clone();
        if !chat.incognito
            && (self.store.workspaces.insert(cwd.clone())
                || self.store.last_cwd.as_ref() != Some(&cwd))
        {
            self.store.last_cwd = Some(cwd);
            self.store.save();
        }
        true
    }

    fn run_task(&mut self, index: usize, task: &ScheduledTask) {
        let key = self.next_chat;
        self.next_chat += 1;
        let mut chat = Chat::new(task.folder.clone());
        chat.title = format!("Scheduled: {}", task.name);
        self.chats.insert(key, chat);
        let prompt = task.prompt.clone();
        self.start_turn(key, &prompt);
        if let Some(chat) = self.chats.get_mut(&key) {
            chat.title = format!("Scheduled: {}", task.name);
        }
        if let Some(stored) = self.store.tasks.get_mut(index) {
            stored.last_run_ms = Some(sessions::now_ms());
        }
        self.store.save();
        self.notice = Some(format!("Started scheduled task \"{}\".", task.name));
    }

    fn run_due_tasks(&mut self) -> Task<Message> {
        use chrono::TimeZone;
        let now = chrono::Local::now();
        let due: Vec<(usize, ScheduledTask)> = self
            .store
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.enabled)
            .filter_map(|(index, task)| {
                let schedule = runtime::scheduler::CronSchedule::parse(&task.schedule).ok()?;
                let since = task.last_run_ms.unwrap_or(self.started_ms);
                let since = chrono::Local
                    .timestamp_millis_opt(i64::try_from(since).ok()?)
                    .single()?;
                let next = schedule.next_after(&since)?;
                (next <= now).then(|| (index, task.clone()))
            })
            .collect();
        for (index, task) in due {
            self.run_task(index, &task);
            notify("Scheduled task started", &task.name);
        }
        Task::none()
    }

    fn on_agent_event(&mut self, event: AgentEvent) -> Task<Message> {
        let key = event.chat;
        let Some(chat) = self.chats.get_mut(&key) else {
            return Task::none();
        };
        let value = &event.event;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match value.get("ev").and_then(Value::as_str).unwrap_or_default() {
            "ready" => {
                chat.session_path = value
                    .get("session_path")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                chat.active_model = Some(field("model"));
                if let Some(path) = chat.session_path.clone() {
                    // Named before it was saved: keep that name.
                    if chat.custom_title {
                        self.store.titles.insert(path.clone(), chat.title.clone());
                    }
                    self.store.app_sessions.insert(path);
                    self.store.save();
                }
            }
            "model" => chat.active_model = Some(field("model")),
            "text" => {
                let text = field("text");
                if let Some(Item::Assistant { content, raw }) = chat.items.last_mut() {
                    content.push_str(&text);
                    raw.push_str(&text);
                } else {
                    let mut content = markdown::Content::new();
                    content.push_str(&text);
                    chat.items.push(Item::Assistant { content, raw: text });
                }
                return self.load_images(key);
            }
            "thinking" => {
                let text = field("text");
                if let Some(Item::Thinking { text: existing, .. }) = chat.items.last_mut() {
                    existing.push_str(&text);
                } else {
                    chat.items.push(Item::Thinking {
                        text,
                        open: self.store.show_thinking,
                    });
                }
            }
            "tool_call" => chat.items.push(Item::Tool {
                name: field("name"),
                input: field("input"),
                output: None,
                is_error: false,
                open: false,
            }),
            "tool_result" => {
                let name = field("name");
                let output = field("output");
                let failed = value.get("is_error").and_then(Value::as_bool) == Some(true);
                if let Some(Item::Tool {
                    output: slot,
                    is_error,
                    open,
                    ..
                }) = chat.items.iter_mut().find(|item| {
                    matches!(item, Item::Tool { name: call, output: None, .. } if *call == name)
                }) {
                    *slot = Some(output);
                    *is_error = failed;
                    // Failures are worth seeing without a click.
                    *open = failed;
                }
            }
            "permission" => {
                chat.pending = Some(Pending::Permission {
                    id: value.get("id").and_then(Value::as_u64).unwrap_or_default(),
                    tool: field("tool"),
                    command: value
                        .get("command")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    reason: value
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
                if !self.focused || key != self.current {
                    notify("claw needs your approval", &chat.title);
                }
            }
            "question" => {
                chat.pending = Some(Pending::Question {
                    id: value.get("id").and_then(Value::as_u64).unwrap_or_default(),
                    question: field("question"),
                    options: value
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
            "error" => chat.items.push(Item::Error(field("message"))),
            "context" => {
                chat.context = Some((
                    value
                        .get("tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    value.get("window").and_then(Value::as_u64),
                ));
            }
            // The agent woke itself: a background job finished or reported.
            "wake" => {
                chat.items.push(Item::Notice(
                    "A background task reported; claw continues".to_string(),
                ));
            }
            "turn_start" => chat.busy = true,
            // A note from the agent between turns (LeanGate, …).
            "notice" => chat.items.push(Item::Notice(field("text"))),
            "background" => {
                chat.background = value
                    .get("jobs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|job| {
                        Some((
                            job.get("id")?.as_str()?.to_string(),
                            job.get("command")?.as_str()?.to_string(),
                        ))
                    })
                    .collect();
            }
            "turn_end" => {
                chat.busy = false;
                chat.pending = None;
                if field("status") == "stopped" {
                    chat.items.push(Item::Notice("Stopped".to_string()));
                }
                if self.store.notify_done && (!self.focused || key != self.current) {
                    notify("Reply ready", &chat.title);
                }
                self.refresh_sessions();
            }
            "exit" => {
                if chat.busy {
                    chat.items.push(Item::Error(
                        "claw stopped unexpectedly. Details: ~/.claw/desktop-agent.log".to_string(),
                    ));
                }
                chat.busy = false;
                chat.pending = None;
                chat.agent = None;
            }
            _ => {}
        }
        Task::none()
    }

    /// Remembers a chat's name for its session file and shows it.
    fn save_title(&mut self, path: PathBuf, title: String) {
        self.store.titles.insert(path, title);
        self.store.save();
        self.refresh_sessions();
    }

    fn save_project(&mut self) -> Task<Message> {
        let name = self.form.name.trim().to_string();
        let path = expand_path(self.form.path.trim());
        if name.is_empty() {
            self.form.error = Some("Give the project a name.".to_string());
            return Task::none();
        }
        if self.form.path.trim().is_empty() {
            self.form.error = Some("Choose a folder for the project.".to_string());
            return Task::none();
        }
        if let Err(error) = std::fs::create_dir_all(&path) {
            self.form.error = Some(format!("Could not create {}: {error}", path.display()));
            return Task::none();
        }
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if let Err(error) = store::write_instructions(&path, &self.form.instructions.text()) {
            self.form.error = Some(format!("Could not save the instructions: {error}"));
            return Task::none();
        }
        let project = Project {
            name,
            path: path.clone(),
        };
        match self.form.editing {
            Some(index) if index < self.store.projects.len() => {
                self.store.projects[index] = project;
            }
            _ => self.store.projects.push(project),
        }
        self.store
            .projects
            .sort_by_key(|project| project.name.to_lowercase());
        self.store.workspaces.insert(path.clone());
        self.store.save();
        self.refresh_sessions();
        let editing = self.form.editing.is_some();
        self.screen = Screen::Chat;
        if !editing {
            self.open_new_chat(path, false);
        }
        Task::none()
    }

    // ---------------------------------------------------------------- view

    fn view(&self) -> Element<'_, Message> {
        let main = match self.screen {
            Screen::Chat => self.chat_view(),
            Screen::ProjectForm => self.project_form_view(),
            Screen::Settings => self.settings_view(),
        };
        let main = container(main)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|theme: &Theme| container::Style {
                background: Some(theme.palette().background.into()),
                ..container::Style::default()
            });
        if self.store.sidebar_hidden {
            main.into()
        } else {
            row![self.sidebar(), rule::vertical(1), main].into()
        }
    }

    fn matches_search(&self, session: &SessionSummary) -> bool {
        let query = self.search.to_lowercase();
        query.is_empty()
            || session.title.to_lowercase().contains(&query)
            || session
                .workspace
                .to_string_lossy()
                .to_lowercase()
                .contains(&query)
    }

    /// The sessions on the current list tab that match the search.
    fn visible_sessions(&self) -> Vec<&SessionSummary> {
        let want_app = self.list_tab == ListTab::Chats;
        self.sessions
            .iter()
            .filter(|session| self.is_app_chat(session) == want_app)
            .filter(|session| self.matches_search(session))
            .take(300)
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    fn sidebar(&self) -> Element<'_, Message> {
        let mut list = Column::new().spacing(2);

        list = list.push(section_header(
            "Projects",
            Some(("+ New", Message::NewProject)),
        ));
        if self.store.projects.is_empty() {
            list = list.push(
                container(
                    text("A project is a folder with its own instructions.")
                        .size(12)
                        .style(theme::muted),
                )
                .padding([0, 6]),
            );
        }
        for (index, project) in self.store.projects.iter().enumerate() {
            let collapsed = self.collapsed_projects.contains(&project.path);
            let arrow = if collapsed { "▸" } else { "▾" };
            let header = row![
                button(text(format!("{arrow}  {}", project.name)).size(14))
                    .style(theme::row_button(false))
                    .width(Length::Fill)
                    .padding([5, 8])
                    .on_press(Message::ToggleProject(project.path.clone())),
                tip(
                    button(text("+").size(15))
                        .style(theme::ghost)
                        .padding([2, 8])
                        .on_press(Message::NewChatIn(project.path.clone())),
                    "New chat in this project",
                    tooltip::Position::Bottom,
                ),
                tip(
                    button(text("⚙").size(13))
                        .style(theme::ghost)
                        .padding([2, 8])
                        .on_press(Message::EditProject(index)),
                    "Project settings",
                    tooltip::Position::Bottom,
                ),
            ]
            .align_y(Alignment::Center);
            list = list.push(header);
            if !collapsed {
                let in_project: Vec<&SessionSummary> = self
                    .sessions
                    .iter()
                    .filter(|session| session.workspace.starts_with(&project.path))
                    .filter(|session| self.matches_search(session))
                    .take(30)
                    .collect();
                if in_project.is_empty() {
                    list = list.push(
                        container(text("No chats yet").size(12).style(theme::muted))
                            .padding([2, 26]),
                    );
                }
                for session in in_project {
                    list = list.push(
                        container(self.session_row(session, false))
                            .padding(iced::Padding::ZERO.left(14)),
                    );
                }
            }
        }

        list = list.push(space::vertical().height(10));
        let tabs = row![
            button(text("Chats").size(13))
                .style(theme::tab(self.list_tab == ListTab::Chats))
                .padding([4, 10])
                .on_press(Message::ListTab(ListTab::Chats)),
            button(text("Terminal sessions").size(13))
                .style(theme::tab(self.list_tab == ListTab::Terminal))
                .padding([4, 10])
                .on_press(Message::ListTab(ListTab::Terminal)),
            space::horizontal(),
            tip(
                button(text(if self.selecting { "Done" } else { "Select" }).size(12))
                    .style(theme::tab(self.selecting))
                    .padding([4, 8])
                    .on_press(Message::ToggleSelecting),
                "Select several to delete",
                tooltip::Position::Bottom,
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        list = list.push(tabs);
        let visible = self.visible_sessions();
        if self.selecting {
            let all = !visible.is_empty()
                && visible
                    .iter()
                    .all(|session| self.selected.contains(&session.path));
            let mut bar = row![checkbox(all)
                .label(format!("All ({})", visible.len()))
                .size(14)
                .text_size(12)
                .on_toggle(|_| Message::SelectAll)]
            .spacing(6)
            .align_y(Alignment::Center)
            .padding([4, 6]);
            bar = bar.push(space::horizontal());
            let mut delete = button(text(format!("Delete {}", self.selected.len())).size(12))
                .style(button::danger)
                .padding([4, 10]);
            if !self.selected.is_empty() {
                delete = delete.on_press(Message::AskDeleteSelected);
            }
            bar = bar.push(delete);
            list = list.push(bar);
            if self.confirm_bulk {
                list = list.push(
                    container(
                        column![
                            text(format!(
                                "Delete {} chat(s)? Their files are removed for good.",
                                self.selected.len()
                            ))
                            .size(13),
                            row![
                                button(text("Delete").size(13))
                                    .style(button::danger)
                                    .on_press(Message::ConfirmDeleteSelected),
                                button(text("Cancel").size(13))
                                    .style(button::secondary)
                                    .on_press(Message::CancelDelete),
                            ]
                            .spacing(6),
                        ]
                        .spacing(6),
                    )
                    .padding(8)
                    .style(theme::card),
                );
            }
        }
        for session in &visible {
            list = list.push(self.session_row(session, true));
        }
        if visible.is_empty() {
            let empty = match self.list_tab {
                ListTab::Chats => "Chats you start here show up here.",
                ListTab::Terminal => "No terminal sessions found. Use \"Find terminal sessions\".",
            };
            list = list.push(container(text(empty).size(12).style(theme::muted)).padding([4, 6]));
        }

        let top = column![
            row![
                button(
                    row![text("+").size(18), text("New chat").size(15)]
                        .spacing(8)
                        .align_y(Alignment::Center)
                )
                .style(button::primary)
                .width(Length::Fill)
                .padding([8, 12])
                .on_press(Message::NewChat),
                tip(
                    button(text("Incognito").size(13))
                        .style(button::secondary)
                        .padding([10, 10])
                        .on_press(Message::NewIncognito),
                    "Incognito chat: nothing is saved (Ctrl+Shift+N)",
                    tooltip::Position::Bottom,
                ),
            ]
            .spacing(6),
            text_input("Search chats", &self.search)
                .on_input(Message::Search)
                .padding(8)
                .size(14),
        ]
        .spacing(10);

        let scan_label = if self.scanning {
            "Searching…"
        } else {
            "Find terminal sessions"
        };
        let mut scan = button(text(scan_label).size(12))
            .style(theme::ghost)
            .padding([4, 6]);
        if !self.scanning {
            scan = scan.on_press(Message::Rescan);
        }
        let bottom = row![
            scan,
            space::horizontal(),
            button(text("Settings").size(12))
                .style(theme::ghost)
                .padding([4, 6])
                .on_press(Message::OpenSettings),
        ]
        .align_y(Alignment::Center);

        container(
            column![
                top,
                scrollable(list.padding(iced::Padding::ZERO.right(10))).height(Length::Fill),
                bottom
            ]
            .spacing(12),
        )
        .padding(12)
        .width(SIDEBAR_WIDTH)
        .height(Length::Fill)
        .style(theme::sidebar)
        .into()
    }

    fn session_row<'a>(
        &'a self,
        session: &'a SessionSummary,
        show_folder: bool,
    ) -> Element<'a, Message> {
        let open = self
            .chats
            .get(&self.current)
            .and_then(|chat| chat.session_path.as_ref())
            == Some(&session.path);
        let running = self
            .chats
            .values()
            .any(|chat| chat.busy && chat.session_path.as_ref() == Some(&session.path));
        if self.confirm_delete.as_ref() == Some(&session.path) {
            return container(
                column![
                    text(format!("Delete \"{}\"?", session.title))
                        .size(13)
                        .wrapping(Wrapping::WordOrGlyph),
                    row![
                        button(text("Delete").size(13))
                            .style(button::danger)
                            .on_press(Message::ConfirmDelete),
                        button(text("Cancel").size(13))
                            .style(button::secondary)
                            .on_press(Message::CancelDelete),
                    ]
                    .spacing(6),
                ]
                .spacing(6),
            )
            .padding(8)
            .style(theme::card)
            .into();
        }
        let app_chat = self.is_app_chat(session);
        let kind = if app_chat { "Chat" } else { "Terminal" };
        let when = sessions::relative_time(session.updated_ms, sessions::now_ms());
        let subtitle = if show_folder {
            format!("{} · {when}", sessions::short_path(&session.workspace))
        } else {
            format!("{kind} · {when}")
        };
        let title = if running {
            format!("● {}", session.title)
        } else {
            session.title.clone()
        };
        // A colored edge tells chats (blue) from terminal sessions (green).
        let edge_color = if app_chat { sol::BLUE } else { sol::GREEN };
        let edge = container(space::horizontal().width(3))
            .height(30)
            .style(move |_: &Theme| container::Style {
                background: Some(edge_color.into()),
                border: iced::border::rounded(2),
                ..container::Style::default()
            });
        let body = column![
            text(title).size(14).wrapping(Wrapping::None),
            text(subtitle)
                .size(11)
                .style(theme::muted)
                .wrapping(Wrapping::None),
        ]
        .spacing(2);
        let content = row![edge, container(body).clip(true).width(Length::Fill)]
            .spacing(8)
            .align_y(Alignment::Center);
        if let Some((path, draft)) = &self.renaming_session {
            if *path == session.path {
                return row![
                    text_input("Chat name", draft)
                        .id("rename-session")
                        .on_input(Message::RenameSessionText)
                        .on_submit(Message::FinishRenameSession)
                        .size(14)
                        .padding(5)
                        .width(Length::Fill),
                    tip(
                        button(text("✓").size(13))
                            .style(theme::ghost)
                            .padding([2, 6])
                            .on_press(Message::FinishRenameSession),
                        "Save the name (Enter)",
                        tooltip::Position::Left,
                    ),
                    tip(
                        button(text("✕").size(12))
                            .style(theme::ghost)
                            .padding([2, 6])
                            .on_press(Message::CancelRename),
                        "Cancel (Esc)",
                        tooltip::Position::Left,
                    ),
                ]
                .spacing(4)
                .align_y(Alignment::Center)
                .into();
            }
        }
        let mut line = row![].align_y(Alignment::Center).spacing(4);
        if self.selecting {
            let checked = self.selected.contains(&session.path);
            let path = session.path.clone();
            line = line.push(
                checkbox(checked)
                    .size(14)
                    .on_toggle(move |on| Message::Select(path.clone(), on)),
            );
        }
        line = line.push(
            button(content)
                .style(theme::row_button(open))
                .width(Length::Fill)
                .padding([5, 6])
                .on_press(Message::OpenSession(session.path.clone())),
        );
        if !self.selecting {
            line = line.push(tip(
                button(text("✎").size(12))
                    .style(theme::ghost)
                    .padding([2, 6])
                    .on_press(Message::StartRenameSession(session.path.clone())),
                "Rename",
                tooltip::Position::Left,
            ));
            line = line.push(tip(
                button(text("✕").size(12))
                    .style(theme::ghost)
                    .padding([2, 6])
                    .on_press(Message::AskDelete(session.path.clone())),
                "Delete chat",
                tooltip::Position::Left,
            ));
        }
        line.into()
    }

    #[allow(clippy::too_many_lines)]
    fn chat_view(&self) -> Element<'_, Message> {
        let Some(chat) = self.chats.get(&self.current) else {
            return text("").into();
        };
        let theme = self.theme();

        // Header: sidebar toggle, title, badges, folder, model.
        let toggle = tip(
            button(text("☰").size(16))
                .style(theme::ghost)
                .padding([4, 8])
                .on_press(Message::ToggleSidebar),
            if self.store.sidebar_hidden {
                "Show the sidebar (Ctrl+B)"
            } else {
                "Hide the sidebar (Ctrl+B)"
            },
            tooltip::Position::Bottom,
        );
        let folder_label = format!("Folder: {}", sessions::short_path(&chat.cwd));
        let can_change_folder = chat.session_path.is_none() && chat.agent.is_none();
        let mut folder = button(text(folder_label).size(13).wrapping(Wrapping::None))
            .style(button::secondary)
            .padding([5, 10]);
        if can_change_folder {
            folder = folder.on_press(Message::PickFolder);
        }
        let folder: Element<'_, Message> = tip(
            folder,
            if can_change_folder {
                "Working folder: click to change"
            } else {
                "A saved chat keeps the folder it started in"
            },
            tooltip::Position::Bottom,
        )
        .into();
        let selected_model = chat
            .model
            .clone()
            .or_else(|| chat.active_model.clone())
            .or_else(|| self.default_model.clone());
        let model_picker = pick_list(self.models.clone(), selected_model, Message::ModelSelected)
            .text_size(13)
            .placeholder("Model");
        let project = self
            .store
            .project_for(&chat.cwd)
            .map(|project| project.name.clone());
        let title: Element<'_, Message> = match &self.renaming {
            Some(draft) => text_input("Chat title", draft)
                .id("rename")
                .on_input(Message::RenameText)
                .on_submit(Message::FinishRename)
                .size(16)
                .padding(4)
                .width(Length::Fill)
                .into(),
            None => {
                let label = row![
                    container(text(chat.title.clone()).size(17).wrapping(Wrapping::None))
                        .clip(true),
                    text("✎").size(13).style(theme::muted),
                ]
                .spacing(6)
                .align_y(Alignment::Center);
                if chat.incognito {
                    label.into()
                } else {
                    tip(
                        button(label)
                            .style(theme::ghost)
                            .padding([2, 4])
                            .on_press(Message::StartRename),
                        "Rename (Enter saves, Esc cancels)",
                        tooltip::Position::Bottom,
                    )
                    .into()
                }
            }
        };
        let mut title_row = row![toggle, title]
            .spacing(8)
            .align_y(Alignment::Center)
            .width(Length::Fill);
        if chat.incognito {
            title_row = title_row.push(
                container(text("Incognito").size(12))
                    .padding([2, 10])
                    .style(theme::badge(sol::VIOLET)),
            );
        }
        if let Some(project) = project {
            title_row = title_row.push(
                container(text(project).size(12))
                    .padding([2, 10])
                    .style(theme::badge(sol::CYAN)),
            );
        }
        let header = container(
            row![title_row, folder, model_picker]
                .spacing(10)
                .align_y(Alignment::Center),
        )
        .padding([8, 14]);

        // Conversation.
        let mut items = Column::new().spacing(16).padding([18, 28]).max_width(940);
        if chat.items.is_empty() {
            let (headline, detail) = if chat.incognito {
                (
                    "Incognito chat",
                    format!(
                        "Nothing from this chat is saved: no session file, no history. It is gone when you close the app. Working in {}.",
                        sessions::short_path(&chat.cwd)
                    ),
                )
            } else {
                (
                    "What can claw do for you?",
                    format!(
                        "Working in {}. claw can read and change files here, run commands and search the web; it asks before sudo and anything destructive.",
                        sessions::short_path(&chat.cwd)
                    ),
                )
            };
            items = items.push(
                column![
                    text(headline).size(26).style(theme::accent(sol::BLUE)),
                    text(detail).size(14).style(theme::muted),
                ]
                .spacing(8)
                .padding([60, 0]),
            );
        }
        let last_assistant = chat
            .items
            .iter()
            .rposition(|item| matches!(item, Item::Assistant { .. }));
        for (index, item) in chat.items.iter().enumerate() {
            let retry = !chat.busy && Some(index) == last_assistant;
            items = items.push(self.item_view(index, item, &theme, retry));
        }
        if chat.busy && chat.pending.is_none() {
            items = items.push(text("claw is working…").size(13).style(theme::muted));
        }
        let conversation = scrollable(container(items).center_x(Length::Fill))
            .anchor_bottom()
            .height(Length::Fill);

        // Waiting for the user.
        let pending: Element<'_, Message> = match &chat.pending {
            Some(Pending::Permission {
                tool,
                command,
                reason,
                ..
            }) => {
                let mut card = column![text(format!("claw wants to run {tool}"))
                    .size(15)
                    .style(theme::accent(sol::YELLOW))]
                .spacing(8);
                if let Some(command) = command {
                    card = card.push(
                        container(
                            text(command.clone())
                                .font(MONO)
                                .size(13)
                                .wrapping(Wrapping::WordOrGlyph),
                        )
                        .padding(8)
                        .width(Length::Fill)
                        .style(theme::code_box),
                    );
                }
                if let Some(reason) = reason {
                    card = card.push(
                        text(reason.clone())
                            .size(13)
                            .style(theme::muted)
                            .wrapping(Wrapping::WordOrGlyph),
                    );
                }
                card = card.push(
                    row![
                        button(text("Allow once"))
                            .style(button::primary)
                            .on_press(Message::Decide("allow")),
                        button(text("Always allow"))
                            .style(button::secondary)
                            .on_press(Message::Decide("always")),
                        button(text("Deny"))
                            .style(button::danger)
                            .on_press(Message::Decide("deny")),
                    ]
                    .spacing(8),
                );
                container(card)
                    .padding(12)
                    .style(theme::card)
                    .width(Length::Fill)
                    .into()
            }
            Some(Pending::Question {
                question, options, ..
            }) => {
                let mut card = column![text(question.clone())
                    .size(15)
                    .wrapping(Wrapping::WordOrGlyph)]
                .spacing(8);
                let mut choices = row![].spacing(8);
                for option in options {
                    choices = choices.push(
                        button(text(option.clone()).size(13))
                            .style(button::secondary)
                            .on_press(Message::Answer(option.clone())),
                    );
                }
                card = card.push(choices);
                card = card.push(
                    row![
                        text_input("Or type an answer", &chat.answer)
                            .on_input(Message::AnswerText)
                            .on_submit(Message::Answer(chat.answer.clone()))
                            .padding(8),
                        button(text("Send")).on_press(Message::Answer(chat.answer.clone())),
                    ]
                    .spacing(8),
                );
                container(card)
                    .padding(12)
                    .style(theme::card)
                    .width(Length::Fill)
                    .into()
            }
            None => space::vertical().height(0).into(),
        };

        // Composer.
        let placeholder = if chat.incognito {
            "Incognito: nothing is saved…"
        } else {
            "Message claw…"
        };
        let editor = text_editor(&self.composer)
            .placeholder(placeholder)
            .on_action(Message::Composer)
            .key_binding(|key_press| {
                let enter = matches!(
                    key_press.key,
                    keyboard::Key::Named(keyboard::key::Named::Enter)
                );
                let focused = matches!(key_press.status, text_editor::Status::Focused { .. });
                if enter && focused && !key_press.modifiers.shift() {
                    Some(text_editor::Binding::Custom(Message::Send))
                } else {
                    text_editor::Binding::from_key_press(key_press)
                }
            })
            .padding(12)
            .size(f32::from(self.store.text_size))
            .min_height(60)
            .max_height(240);
        let action: Element<'_, Message> = if chat.busy {
            button(text("Stop").size(14))
                .style(button::danger)
                .padding([10, 16])
                .on_press(Message::Stop)
                .into()
        } else {
            button(text("Send").size(14))
                .style(button::primary)
                .padding([10, 16])
                .on_press(Message::Send)
                .into()
        };
        let attach = tip(
            button(text("Attach").size(13))
                .style(button::secondary)
                .padding([10, 10])
                .on_press(Message::Attach),
            "Add files to the message",
            tooltip::Position::Top,
        );
        let status = {
            let mut parts = Vec::new();
            if let Some(model) = &chat.active_model {
                parts.push(model.clone());
            }
            if let Some((tokens, window)) = chat.context {
                parts.push(match window {
                    Some(window) => format!(
                        "{:.1}k / {}k context",
                        tokens as f64 / 1000.0,
                        window / 1000
                    ),
                    None => format!("{:.1}k context", tokens as f64 / 1000.0),
                });
            }
            if let Some(notice) = &self.notice {
                parts.push(notice.clone());
            }
            row![
                text(parts.join("  ·  "))
                    .size(12)
                    .style(theme::muted)
                    .width(Length::Fill),
                keys("Enter", 11),
                text("send").size(12).style(theme::muted),
                keys("Shift+Enter", 11),
                text("new line").size(12).style(theme::muted),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
        };
        let background: Element<'_, Message> = if chat.background.is_empty() {
            space::vertical().height(0).into()
        } else {
            let mut jobs = column![row![
                text(format!(
                    "⚙ Running in the background ({})",
                    chat.background.len()
                ))
                .size(12)
                .style(theme::accent(sol::YELLOW))
                .width(Length::Fill),
                button(text("Stop all").size(12))
                    .style(button::danger)
                    .padding([2, 10])
                    .on_press(Message::StopBackground(None)),
            ]
            .align_y(Alignment::Center)]
            .spacing(4);
            for (id, command) in chat.background.iter().take(4) {
                jobs = jobs.push(
                    row![
                        container(
                            text(command.clone())
                                .size(12)
                                .font(MONO)
                                .style(theme::muted)
                                .wrapping(Wrapping::None),
                        )
                        .clip(true)
                        .width(Length::Fill),
                        button(text("Stop").size(11))
                            .style(theme::ghost)
                            .padding([1, 8])
                            .on_press(Message::StopBackground(Some(id.clone()))),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                );
            }
            container(jobs).padding([6, 10]).style(theme::card).into()
        };
        let editing: Element<'_, Message> = if chat.editing.is_some() {
            row![
                text("✎ Editing a message: sending replaces it and everything after it")
                    .size(12)
                    .style(theme::accent(sol::CYAN))
                    .width(Length::Fill),
                button(text("Cancel").size(12))
                    .style(theme::ghost)
                    .padding([2, 8])
                    .on_press(Message::CancelEdit),
            ]
            .align_y(Alignment::Center)
            .into()
        } else {
            space::vertical().height(0).into()
        };
        let composer = column![
            pending,
            background,
            editing,
            row![attach, editor, action]
                .spacing(8)
                .align_y(Alignment::End),
            status,
        ]
        .spacing(8)
        .padding([10, 28])
        .max_width(996);

        column![
            header,
            rule::horizontal(1),
            conversation,
            container(composer).center_x(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// The message at `index` as selectable text, when it is switched to that.
    fn selectable(&self, index: usize, size: f32) -> Option<Element<'_, Message>> {
        let (chat, item, content) = self.select.as_ref()?;
        (*chat == self.current && *item == index).then(|| {
            text_editor(content)
                .on_action(Message::SelectAction)
                .size(size)
                .wrapping(Wrapping::WordOrGlyph)
                .into()
        })
    }

    fn select_button(&self, index: usize) -> Element<'_, Message> {
        let selecting = self
            .select
            .as_ref()
            .is_some_and(|(chat, item, _)| *chat == self.current && *item == index);
        tip(
            button(
                text(if selecting { "Done" } else { "Select text" })
                    .size(11)
                    .style(theme::muted),
            )
            .style(theme::ghost)
            .padding([2, 6])
            .on_press(Message::SelectText(index)),
            if selecting {
                "Back to the formatted view"
            } else {
                "Show as plain text you can select with the mouse (Ctrl+C copies). Double-click a message does the same"
            },
            tooltip::Position::Top,
        )
        .into()
    }

    #[allow(clippy::too_many_lines)]
    fn item_view<'a>(
        &'a self,
        index: usize,
        item: &'a Item,
        theme: &Theme,
        retry: bool,
    ) -> Element<'a, Message> {
        let size = f32::from(self.store.text_size);
        match item {
            Item::User(text_content) => {
                let body: Element<'a, Message> = match self.selectable(index, size) {
                    Some(editor) => editor,
                    None => text(text_content.as_str())
                        .size(size)
                        .wrapping(Wrapping::WordOrGlyph)
                        .into(),
                };
                let busy = self.chats.get(&self.current).is_some_and(|chat| chat.busy);
                let mut edit = button(text("Edit").size(11).style(theme::muted))
                    .style(theme::ghost)
                    .padding([2, 6]);
                if !busy {
                    edit = edit.on_press(Message::EditPrompt(index));
                }
                column![
                    row![
                        space::horizontal().width(Length::FillPortion(1)),
                        mouse_area(
                            container(body)
                                .padding([10, 14])
                                .style(theme::user_bubble)
                                .max_width(700),
                        )
                        .on_double_click(Message::SelectText(index)),
                    ],
                    row![
                        space::horizontal(),
                        button(text("Copy").size(11).style(theme::muted))
                            .style(theme::ghost)
                            .padding([2, 6])
                            .on_press(Message::Copy(text_content.clone())),
                        tip(
                            edit,
                            "Change this message and send it again; the replies after it are replaced",
                            tooltip::Position::Top,
                        ),
                        self.select_button(index),
                    ]
                    .spacing(4),
                ]
                .spacing(2)
                .into()
            }
            Item::Assistant { content, raw } => {
                let palette = theme.palette();
                let style = markdown::Style {
                    font: Font::default(),
                    inline_code_highlight: iced::advanced::text::Highlight {
                        background: theme::code_color(theme).into(),
                        border: iced::border::rounded(4),
                    },
                    inline_code_padding: iced::Padding::from([0, 3]),
                    inline_code_color: if theme::is_light(theme) {
                        sol::MAGENTA
                    } else {
                        sol::CYAN
                    },
                    inline_code_font: MONO,
                    code_block_font: MONO,
                    link_color: palette.primary,
                };
                let mut settings = markdown::Settings::with_text_size(size, style);
                settings.code_size = (size - 1.5).into();
                settings.h1_size = (size * 1.6).into();
                settings.h2_size = (size * 1.4).into();
                settings.h3_size = (size * 1.2).into();
                let viewer = markdown_view::Viewer {
                    images: &self.images,
                    saved: &self.saved,
                    highlights: &self.highlights,
                    light: theme::is_light(theme),
                };
                let mut actions = row![
                    button(text("Copy").size(11).style(theme::muted))
                        .style(theme::ghost)
                        .padding([2, 6])
                        .on_press(Message::Copy(raw.clone())),
                    self.select_button(index),
                ]
                .spacing(4);
                if retry {
                    actions = actions.push(
                        button(text("Retry").size(11).style(theme::muted))
                            .style(theme::ghost)
                            .padding([2, 6])
                            .on_press(Message::Retry),
                    );
                }
                let body = self.selectable(index, size).unwrap_or_else(|| {
                    mouse_area(markdown::view_with(content.items(), settings, &viewer))
                        .on_double_click(Message::SelectText(index))
                        .into()
                });
                column![body, actions].spacing(4).into()
            }
            Item::Thinking {
                text: thought,
                open,
            } => {
                let header = button(
                    text(if *open {
                        "▾ Thinking"
                    } else {
                        "▸ Thinking"
                    })
                    .size(13)
                    .style(theme::accent(sol::VIOLET)),
                )
                .style(theme::ghost)
                .padding([2, 4])
                .on_press(Message::ToggleItem(index));
                if *open {
                    column![
                        header,
                        container(
                            text(thought.as_str())
                                .size(13)
                                .style(theme::muted)
                                .wrapping(Wrapping::WordOrGlyph)
                        )
                        .padding(iced::Padding::ZERO.left(12))
                    ]
                    .spacing(6)
                    .into()
                } else {
                    header.into()
                }
            }
            Item::Tool {
                name,
                input,
                output,
                is_error,
                open,
            } => {
                let (state, state_color) = match (output, is_error) {
                    (None, _) => ("running", sol::YELLOW),
                    (Some(_), true) => ("failed", sol::RED),
                    (Some(_), false) => ("done", sol::GREEN),
                };
                let arrow = if *open { "▾" } else { "▸" };
                let summary = sessions::tool_summary(name, input);
                let header = button(
                    row![
                        text(arrow).size(12).style(theme::muted),
                        text(name.clone())
                            .size(13)
                            .font(MONO)
                            .style(theme::accent(sol::BLUE)),
                        container(
                            text(summary)
                                .size(13)
                                .font(MONO)
                                .style(theme::muted)
                                .wrapping(Wrapping::None),
                        )
                        .clip(true)
                        .width(Length::Fill),
                        text(state).size(12).style(theme::accent(state_color)),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .style(theme::ghost)
                .width(Length::Fill)
                .padding([5, 8])
                .on_press(Message::ToggleItem(index));
                let mut block = column![header].spacing(4);
                if *open {
                    let shown = output.clone().unwrap_or_else(|| "running…".to_string());
                    let body = column![
                        colored_output(&shown, size - 2.0),
                        row![
                            space::horizontal(),
                            button(text("Copy").size(11))
                                .style(theme::ghost)
                                .padding([2, 6])
                                .on_press(Message::Copy(shown.clone())),
                        ],
                    ];
                    block = block.push(container(body).padding(10).width(Length::Fill).style(
                        if *is_error {
                            theme::error_box
                        } else {
                            theme::code_box
                        },
                    ));
                }
                container(block)
                    .style(theme::card)
                    .padding(3)
                    .width(Length::Fill)
                    .into()
            }
            Item::Notice(notice) => text(notice.as_str()).size(12).style(theme::muted).into(),
            Item::Error(error) => container(
                text(error.as_str())
                    .size(13)
                    .wrapping(Wrapping::WordOrGlyph),
            )
            .padding(10)
            .width(Length::Fill)
            .style(theme::error_box)
            .into(),
        }
    }

    fn project_form_view(&self) -> Element<'_, Message> {
        let form = &self.form;
        let heading = if form.editing.is_some() {
            "Project settings"
        } else {
            "New project"
        };
        let mut content = column![
            row![sidebar_toggle(self.store.sidebar_hidden), text(heading).size(24)]
                .spacing(8)
                .align_y(Alignment::Center),
            text("A project is a working folder. Its instructions are saved in <folder>/.claw/instructions.md, so claw follows them in every chat there, in the app and in the terminal.")
                .size(13)
                .style(theme::muted),
            text("Name").size(14),
            text_input("My project", &form.name)
                .on_input(Message::ProjectName)
                .padding(8),
            text("Folder (created if it does not exist)").size(14),
            row![
                text_input("~/ws/my-project", &form.path)
                    .on_input(Message::ProjectPath)
                    .padding(8),
                button(text("Browse…")).style(button::secondary).on_press(Message::ProjectBrowse),
            ]
            .spacing(8),
            text("Instructions (optional)").size(14),
            text_editor(&form.instructions)
                .placeholder("How claw should work here: conventions, commands to run, things to avoid…")
                .on_action(Message::ProjectInstructions)
                .padding(10)
                .height(220),
        ]
        .spacing(10)
        .max_width(760);
        if let Some(error) = &form.error {
            content = content.push(text(error.clone()).style(text::danger));
        }
        let mut buttons = row![
            button(text(if form.editing.is_some() {
                "Save"
            } else {
                "Create project"
            }))
            .style(button::primary)
            .on_press(Message::SaveProject),
            button(text("Cancel"))
                .style(button::secondary)
                .on_press(Message::CancelProject),
        ]
        .spacing(8);
        if let Some(index) = form.editing {
            buttons = buttons.push(space::horizontal());
            if self.confirm_delete_project == Some(index) {
                buttons =
                    buttons.push(text("Remove from the app? The folder and chats stay.").size(13));
                buttons = buttons.push(
                    button(text("Remove"))
                        .style(button::danger)
                        .on_press(Message::ConfirmDeleteProject),
                );
                buttons = buttons.push(
                    button(text("Keep"))
                        .style(button::secondary)
                        .on_press(Message::CancelDelete),
                );
            } else {
                buttons = buttons.push(
                    button(text("Remove project"))
                        .style(button::danger)
                        .on_press(Message::AskDeleteProject(index)),
                );
            }
        }
        content = content.push(buttons);
        scrollable(container(content).padding(32).center_x(Length::Fill))
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let state = &self.settings;
        let mut tabs = row![].spacing(4);
        for tab in SettingsTab::ALL {
            tabs = tabs.push(
                button(text(tab.label()).size(14))
                    .style(theme::tab(state.tab == tab))
                    .padding([6, 12])
                    .on_press(Message::SettingsTab(tab)),
            );
        }
        let body = match state.tab {
            SettingsTab::General => self.general_settings(),
            SettingsTab::Skills => self.skills_settings(),
            SettingsTab::Mcp => self.mcp_settings(),
            SettingsTab::Tools => self.tools_settings(),
            SettingsTab::Schedule => self.schedule_settings(),
        };
        let mut content = column![
            row![
                sidebar_toggle(self.store.sidebar_hidden),
                text("Settings").size(24),
                space::horizontal(),
                button(text("Back to chat"))
                    .style(button::secondary)
                    .on_press(Message::CloseSettings),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            tabs,
            rule::horizontal(1),
        ]
        .spacing(12)
        .max_width(860);
        if let Some((ok, notice)) = &state.notice {
            content = content.push(
                container(
                    text(notice.clone())
                        .size(13)
                        .wrapping(Wrapping::WordOrGlyph),
                )
                .padding(8)
                .width(Length::Fill)
                .style(if *ok { theme::card } else { theme::error_box }),
            );
        }
        content = content.push(body);
        scrollable(container(content).padding(28).center_x(Length::Fill))
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    fn general_settings(&self) -> Element<'_, Message> {
        let folder = sessions::short_path(&self.store.new_chat_folder());
        column![
            setting_title("Default folder for new chats"),
            text("Projects and the folder button override it per chat. ~/ws/tmp is a RAM disk: good for scratch work, emptied on reboot.")
                .size(13)
                .style(theme::muted),
            row![
                container(text(folder).font(MONO).size(13))
                    .padding(8)
                    .style(theme::code_box),
                button(text("Change…"))
                    .style(button::secondary)
                    .on_press(Message::DefaultFolderBrowse),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            setting_title("Appearance"),
            row![
                text(if self.store.light_theme {
                    "Solarized light"
                } else {
                    "Solarized dark"
                })
                .size(14),
                button(text("Switch theme").size(13))
                    .style(button::secondary)
                    .on_press(Message::ToggleTheme),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
            row![
                text(format!("Text size: {}", self.store.text_size)).size(14),
                button(text("A−").size(13))
                    .style(button::secondary)
                    .on_press(Message::SetTextSize(self.store.text_size.saturating_sub(1))),
                button(text("A+").size(13))
                    .style(button::secondary)
                    .on_press(Message::SetTextSize(self.store.text_size + 1)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            setting_title("Replies"),
            toggler(self.store.notify_done)
                .label("Notify me when a reply finishes or claw needs approval while I am elsewhere")
                .on_toggle(Message::SetNotify),
            toggler(self.store.show_thinking)
                .label("Show the model's thinking expanded")
                .on_toggle(Message::SetShowThinking),
            setting_title("Keyboard"),
            container(
                column(SHORTCUTS.iter().map(|(combo, label)| {
                    row![
                        container(keys(combo, 13)).width(Length::Fixed(190.0)),
                        text(*label).size(13),
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center)
                    .into()
                }))
                .spacing(10),
            )
            .padding([12, 14])
            .style(theme::card),
        ]
        .spacing(10)
        .into()
    }

    fn skills_settings(&self) -> Element<'_, Message> {
        let state = &self.settings;
        let mut install = button(text(if state.installing {
            "Installing…"
        } else {
            "Install"
        }))
        .style(button::primary);
        if !state.installing {
            install = install.on_press(Message::InstallSkill);
        }
        let mut list = column![
            setting_title("Install a skill"),
            text("A skill is a folder with a SKILL.md that tells claw how to do one kind of task. Install from a folder, a SKILL.md or .md file, a .zip/.skill archive, or a git URL. Skills go to ~/.claw/skills and work in the terminal too.")
                .size(13)
                .style(theme::muted),
            row![
                text_input("Folder, file or https://github.com/… URL", &state.skill_source)
                    .on_input(Message::SkillSource)
                    .on_submit(Message::InstallSkill)
                    .padding(8),
                button(text("Folder…"))
                    .style(button::secondary)
                    .on_press(Message::SkillBrowseFolder),
                button(text("File…"))
                    .style(button::secondary)
                    .on_press(Message::SkillBrowseFile),
                install,
            ]
            .spacing(8),
            row![
                setting_title(&format!("Installed ({})", state.skills.len())),
                space::horizontal(),
                button(text("Open skills folder").size(13))
                    .style(theme::ghost)
                    .on_press(Message::OpenFolder(settings::skills_dir())),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        if state.skills.is_empty() {
            list = list.push(
                text("No skills installed yet.")
                    .size(13)
                    .style(theme::muted),
            );
        }
        for skill in &state.skills {
            let confirming = state.confirm_skill.as_ref() == Some(&skill.dir);
            let actions: Element<'_, Message> = if confirming {
                row![
                    button(text("Remove").size(13))
                        .style(button::danger)
                        .on_press(Message::ConfirmDeleteSkill),
                    button(text("Keep").size(13))
                        .style(button::secondary)
                        .on_press(Message::CancelDelete),
                ]
                .spacing(6)
                .into()
            } else {
                row![
                    button(text("Open").size(13))
                        .style(theme::ghost)
                        .on_press(Message::OpenFolder(skill.dir.clone())),
                    button(text("Remove").size(13))
                        .style(theme::ghost)
                        .on_press(Message::AskDeleteSkill(skill.dir.clone())),
                ]
                .spacing(6)
                .into()
            };
            list = list.push(
                container(
                    row![
                        column![
                            text(skill.name.clone())
                                .size(15)
                                .style(theme::accent(sol::BLUE)),
                            text(skill.description.clone())
                                .size(13)
                                .wrapping(Wrapping::WordOrGlyph),
                            text(sessions::short_path(&skill.dir))
                                .size(11)
                                .style(theme::muted),
                        ]
                        .spacing(3)
                        .width(Length::Fill),
                        actions,
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding(10)
                .style(theme::card),
            );
        }
        list.into()
    }

    fn mcp_settings(&self) -> Element<'_, Message> {
        let state = &self.settings;
        let mut list = column![
            setting_title("Connect an MCP server"),
            text("MCP servers give claw more tools (databases, browsers, APIs). They are saved in ~/.claw/settings.json, so the terminal claw uses them too. New chats connect to them.")
                .size(13)
                .style(theme::muted),
            row![
                text_input("Name, e.g. filesystem", &state.mcp_name)
                    .on_input(Message::McpName)
                    .padding(8)
                    .width(Length::FillPortion(1)),
                text_input("Command (npx -y @modelcontextprotocol/server-filesystem ~/ws) or https:// URL", &state.mcp_target)
                    .on_input(Message::McpTarget)
                    .padding(8)
                    .width(Length::FillPortion(3)),
            ]
            .spacing(8),
            text_editor(&state.mcp_env)
                .placeholder("Optional: environment, one KEY=value per line (for a URL: headers, one Name: value per line)")
                .on_action(Message::McpEnv)
                .padding(8)
                .height(80),
            row![button(text("Add server"))
                .style(button::primary)
                .on_press(Message::AddMcp)],
            setting_title(&format!("Servers ({})", state.mcp.len())),
        ]
        .spacing(10);
        if state.mcp.is_empty() {
            list = list.push(text("No MCP servers yet.").size(13).style(theme::muted));
        }
        for server in &state.mcp {
            list = list.push(
                container(
                    row![
                        column![
                            row![
                                text(server.name.clone())
                                    .size(15)
                                    .style(theme::accent(sol::BLUE)),
                                container(
                                    text(if server.remote { "remote" } else { "local" }).size(11)
                                )
                                .padding([1, 8])
                                .style(theme::badge(
                                    if server.remote {
                                        sol::MAGENTA
                                    } else {
                                        sol::GREEN
                                    }
                                )),
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center),
                            text(server.summary.clone())
                                .font(MONO)
                                .size(12)
                                .style(theme::muted)
                                .wrapping(Wrapping::WordOrGlyph),
                        ]
                        .spacing(3)
                        .width(Length::Fill),
                        button(text("Remove").size(13))
                            .style(theme::ghost)
                            .on_press(Message::RemoveMcp(server.name.clone())),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding(10)
                .style(theme::card),
            );
        }
        list.into()
    }

    fn tools_settings(&self) -> Element<'_, Message> {
        let state = &self.settings;
        let mut list = column![
            setting_title("Built-in tools"),
            text("Turn a tool off and claw no longer sees or uses it (saved as permissions.deniedTools in ~/.claw/settings.json; applies to new chats and the terminal). Approval rules for commands stay as they are.")
                .size(13)
                .style(theme::muted),
        ]
        .spacing(8);
        for (tool, description) in settings::tools() {
            let enabled = !state.denied.contains(&tool);
            let name = tool.clone();
            list = list.push(
                container(
                    row![
                        column![
                            text(tool).font(MONO).size(14),
                            text(description).size(12).style(theme::muted),
                        ]
                        .spacing(2)
                        .width(Length::Fill),
                        toggler(enabled).on_toggle(move |on| Message::ToggleTool(name.clone(), on)),
                    ]
                    .align_y(Alignment::Center),
                )
                .padding([6, 10])
                .style(theme::card),
            );
        }
        list.into()
    }

    fn schedule_settings(&self) -> Element<'_, Message> {
        let state = &self.settings;
        let mut list = column![
            setting_title("Schedule a task"),
            text("claw runs the prompt in a new chat on schedule while this app is open (it does not run when the app is closed). The schedule is cron: minute hour day month weekday. Examples: \"0 9 * * *\" every day at 9:00, \"*/30 * * * *\" every 30 minutes, \"0 18 * * 1-5\" weekdays at 18:00, or @hourly / @daily / @weekly.")
                .size(13)
                .style(theme::muted),
            row![
                text_input("Name, e.g. Morning update check", &state.task_name)
                    .on_input(Message::TaskName)
                    .padding(8)
                    .width(Length::FillPortion(2)),
                text_input("0 9 * * *", &state.task_schedule)
                    .on_input(Message::TaskSchedule)
                    .font(MONO)
                    .padding(8)
                    .width(Length::FillPortion(1)),
            ]
            .spacing(8),
            row![
                text_input(
                    &format!("Folder (default {})", sessions::short_path(&self.store.new_chat_folder())),
                    &state.task_folder
                )
                .on_input(Message::TaskFolder)
                .padding(8),
                button(text("Browse…"))
                    .style(button::secondary)
                    .on_press(Message::TaskBrowse),
            ]
            .spacing(8),
            text_editor(&state.task_prompt)
                .placeholder("What claw should do, e.g. \"Check for Gentoo news and pending updates (pretend run only) and summarise them.\"")
                .on_action(Message::TaskPrompt)
                .padding(8)
                .height(100),
            row![button(text("Add task"))
                .style(button::primary)
                .on_press(Message::AddTask)],
            setting_title(&format!("Tasks ({})", self.store.tasks.len())),
        ]
        .spacing(10);
        if self.store.tasks.is_empty() {
            list = list.push(text("No scheduled tasks yet.").size(13).style(theme::muted));
        }
        for (index, task) in self.store.tasks.iter().enumerate() {
            let last = task
                .last_run_ms
                .map(|ms| sessions::relative_time(ms, sessions::now_ms()))
                .unwrap_or_else(|| "never".to_string());
            list = list.push(
                container(
                    row![
                        column![
                            row![
                                text(task.name.clone())
                                    .size(15)
                                    .style(theme::accent(sol::BLUE)),
                                container(text(task.schedule.clone()).font(MONO).size(11))
                                    .padding([1, 8])
                                    .style(theme::badge(sol::ORANGE)),
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center),
                            text(task.prompt.clone())
                                .size(13)
                                .wrapping(Wrapping::WordOrGlyph),
                            text(format!(
                                "{} · last run {last}",
                                sessions::short_path(&task.folder)
                            ))
                            .size(11)
                            .style(theme::muted),
                        ]
                        .spacing(3)
                        .width(Length::Fill),
                        toggler(task.enabled).on_toggle(move |on| Message::ToggleTask(index, on)),
                        button(text("Run now").size(13))
                            .style(theme::ghost)
                            .on_press(Message::RunTask(index)),
                        button(text("Delete").size(13))
                            .style(theme::ghost)
                            .on_press(Message::DeleteTask(index)),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding(10)
                .style(theme::card),
            );
        }
        list.into()
    }
}

fn sidebar_toggle(hidden: bool) -> Element<'static, Message> {
    tip(
        button(text("☰").size(16))
            .style(theme::ghost)
            .padding([4, 8])
            .on_press(Message::ToggleSidebar),
        if hidden {
            "Show the sidebar (Ctrl+B)"
        } else {
            "Hide the sidebar (Ctrl+B)"
        },
        tooltip::Position::Bottom,
    )
    .into()
}

/// A hover hint in a bubble (iced draws a bare tooltip as text with no
/// background, over whatever is below it).
fn tip<'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<String>,
    position: tooltip::Position,
) -> tooltip::Tooltip<'a, Message> {
    tooltip(
        content,
        container(text(label.into()).size(12))
            .padding([5, 10])
            .style(theme::tooltip_bubble),
        position,
    )
    .gap(6)
}

/// The app's keyboard shortcuts, listed under Keyboard in the general settings.
const SHORTCUTS: &[(&str, &str)] = &[
    ("Enter", "Send the message"),
    ("Shift+Enter", "New line in the message"),
    ("Ctrl+N", "New chat"),
    ("Ctrl+Shift+N", "New incognito chat"),
    ("Ctrl+B", "Show or hide the sidebar"),
    ("Ctrl+,", "Settings"),
    (
        "Ctrl+Q",
        "Quit (ends the chats' agents; the local model server stops when no claw uses it)",
    ),
];

/// A shortcut such as `Ctrl+Shift+N` drawn as one keycap per key.
fn keys(combo: &str, size: u32) -> Element<'static, Message> {
    let caps = combo.split('+').map(|key| {
        // `Ctrl+,` splits into "Ctrl" and ","; a lone "+" key stays itself.
        let key = if key.is_empty() { "+" } else { key };
        container(text(key.to_string()).font(MONO).size(size))
            .padding([2, 7])
            .style(theme::keycap)
            .into()
    });
    row(caps).spacing(4).align_y(Alignment::Center).into()
}

fn setting_title(label: &str) -> Element<'static, Message> {
    container(
        text(label.to_string())
            .size(16)
            .style(theme::accent(sol::YELLOW)),
    )
    .padding(iced::Padding::ZERO.top(8))
    .into()
}

fn section_header(label: &str, action: Option<(&str, Message)>) -> Element<'static, Message> {
    let mut header = row![text(label.to_uppercase()).size(11).style(theme::muted)]
        .align_y(Alignment::Center)
        .padding([4, 6]);
    if let Some((label, message)) = action {
        header = header.push(space::horizontal());
        header = header.push(
            button(text(label.to_string()).size(12))
                .style(theme::ghost)
                .padding([2, 6])
                .on_press(message),
        );
    }
    header.into()
}

/// Tool output with diff lines, errors and the exit status colored.
fn colored_output(output: &str, size: f32) -> Element<'static, Message> {
    let lines: Vec<&str> = output.lines().take(2000).collect();
    let spans: Vec<iced::widget::text::Span<'static, ()>> = lines
        .iter()
        .flat_map(|line| {
            let lower = line.to_ascii_lowercase();
            let color = if line.starts_with("+++") || line.starts_with("---") {
                Some(sol::BASE1)
            } else if line.starts_with('+') {
                Some(sol::GREEN)
            } else if line.starts_with('-') {
                Some(sol::RED)
            } else if line.starts_with("@@") {
                Some(sol::VIOLET)
            } else if line.starts_with("[exit code 0]") {
                Some(sol::GREEN)
            } else if line.starts_with("[exit code") || line.starts_with("[stderr]") {
                Some(sol::ORANGE)
            } else if line.starts_with("[claw]") || line.starts_with("[sandbox]") {
                Some(sol::YELLOW)
            } else if lower.contains("error") || lower.starts_with("!!!") {
                Some(sol::RED)
            } else if lower.contains("warning") {
                Some(sol::YELLOW)
            } else {
                None
            };
            let mut first = span(line.to_string());
            if let Some(color) = color {
                first = first.color(color);
            }
            [first, span("\n")]
        })
        .collect();
    rich_text(spans)
        .font(MONO)
        .size(size)
        .wrapping(Wrapping::WordOrGlyph)
        .into()
}

/// A desktop notification (`notify-send`), when available.
fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Claw", "-i", "claw", summary, body])
        .spawn();
}

fn scan_task() -> Task<Message> {
    Task::perform(
        async {
            tokio::task::spawn_blocking(|| sessions::scan_workspaces(&sessions::home_dir(), 6))
                .await
                .unwrap_or_default()
        },
        Message::Scanned,
    )
}

async fn pick_folder(start: PathBuf) -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_directory(&start)
        .set_title("Choose a folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
}

async fn pick_files(start: PathBuf) -> Vec<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_directory(&start)
        .set_title("Attach files")
        .pick_files()
        .await
        .map(|handles| {
            handles
                .iter()
                .map(|handle| handle.path().to_path_buf())
                .collect()
        })
        .unwrap_or_default()
}

async fn pick_skill_file() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_directory(sessions::home_dir())
        .set_title("Choose a skill file")
        .add_filter("Skill", &["md", "zip", "skill"])
        .pick_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

/// `~/x` -> `$HOME/x`; relative paths are taken from the home folder.
pub fn expand_path(path: &str) -> PathBuf {
    let home = sessions::home_dir();
    if path == "~" {
        home
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        home.join(path)
    }
}
