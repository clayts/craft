//! The header bar's button for the tools Craft runs for a window: language
//! servers, and direnv. Its icon shows how they are, all told: a tick while
//! all is well, a spinner while any is busy, and a warning sign once any has
//! gone wrong. It opens onto a list of the tools, each with its own icon,
//! from which each tool's log opens.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{glib, pango};
use lsp_types::{NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress};

/// How long a tool stays busy after its last work ends, in case more begins,
/// as it does between the stages of a server's start.
const LINGER: Duration = Duration::from_millis(500);

/// How many entries each tool's log keeps, dropping the oldest.
const LOG_LENGTH: usize = 100;

/// Stands for the client of work that is not a server's.
const NO_CLIENT: usize = usize::MAX;

/// How much an entry in a log matters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
}

/// How a tool is, or how the tools are, all told, from best to worst.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Idle,
    Busy,
    Failed,
}

pub struct Status {
    button: gtk::MenuButton,
    icon: Icon,
    pages: gtk::Stack,
    list: gtk::ListBox,
    page: LogPage,
    /// The tools, in the order they first came up, as their rows are.
    tools: RefCell<Vec<Tool>>,
    /// The tool whose log shows, by index.
    shown: Cell<Option<usize>>,
    /// Numbers the work that is not a server's.
    next_token: Cell<i32>,
}

struct Tool {
    name: String,
    row: adw::ActionRow,
    icon: Icon,
    /// Work under way, in the order it began.
    works: Vec<Work>,
    /// Whether the tool shows as busy, which it goes on doing for a moment
    /// after its work ends.
    busy: bool,
    /// Whether an error has been logged since the tool last started afresh,
    /// or its log was cleared.
    failed: bool,
    log: VecDeque<Entry>,
}

/// Work that a tool reports progress on.
struct Work {
    client: usize,
    token: NumberOrString,
    title: String,
    message: Option<String>,
    percentage: Option<u32>,
}

struct Entry {
    level: Level,
    time: String,
    message: String,
}

/// The page showing a tool's log, below a header to go back from it, with
/// the tool's name, and to clear the log.
struct LogPage {
    root: gtk::Box,
    back: gtk::Button,
    title: gtk::Label,
    clear: gtk::Button,
    /// The tool's work under way, a line each.
    work: gtk::Label,
    entries: gtk::Box,
}

/// A tick, a spinner or a warning sign, showing a state.
struct Icon {
    stack: gtk::Stack,
    spinner: gtk::Spinner,
}

/// Work that a tool does while this lives, showing the tool as busy.
pub struct Busy {
    status: Weak<Status>,
    tool: usize,
    token: NumberOrString,
}

impl Status {
    pub fn new() -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        let pages = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::SlideLeftRight)
            .vhomogeneous(false)
            .build();
        let page = LogPage::new();
        pages.add_named(&list, Some("tools"));
        pages.add_named(&page.root, Some("log"));
        let popover = gtk::Popover::builder()
            .child(
                &adw::Clamp::builder()
                    .maximum_size(420)
                    .tightening_threshold(420)
                    .child(&pages)
                    .build(),
            )
            .build();
        let icon = Icon::new();
        let button = gtk::MenuButton::builder()
            .child(&icon.stack)
            .tooltip_text("Language Servers")
            .popover(&popover)
            .visible(false)
            .build();

        let status = Rc::new(Self {
            button,
            icon,
            pages,
            list,
            page,
            tools: RefCell::default(),
            shown: Cell::new(None),
            next_token: Cell::new(0),
        });
        let weak = Rc::downgrade(&status);
        status.list.connect_row_activated(glib::clone!(
            #[strong]
            weak,
            move |_, row| {
                if let (Some(status), Ok(index)) = (weak.upgrade(), usize::try_from(row.index())) {
                    status.open(index);
                }
            }
        ));
        status.page.back.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(status) = weak.upgrade() {
                    status.close(gtk::StackTransitionType::SlideLeftRight);
                }
            }
        ));
        status.page.clear.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(status) = weak.upgrade() {
                    status.clear();
                }
            }
        ));
        popover.connect_closed(move |_| {
            if let Some(status) = weak.upgrade() {
                status.close(gtk::StackTransitionType::None);
            }
        });
        status
    }

    pub fn widget(&self) -> &gtk::MenuButton {
        &self.button
    }

    /// Logs `message` from `tool`, if it says anything. An error shows on
    /// the tool's icon until it starts afresh, or its log is cleared.
    pub fn log(&self, tool: &str, level: Level, message: &str) {
        let message = message.trim();
        if message.is_empty() {
            return;
        }
        let index = self.tool(tool);
        {
            let mut tools = self.tools.borrow_mut();
            let tool = &mut tools[index];
            tool.failed |= level == Level::Error;
            // A repeat shows once.
            let repeat = tool
                .log
                .back()
                .is_some_and(|last| last.level == level && last.message == message);
            if !repeat {
                let shown = self.shown.get() == Some(index);
                if tool.log.len() == LOG_LENGTH {
                    tool.log.pop_front();
                    if let Some(oldest) = self.page.entries.first_child().filter(|_| shown) {
                        self.page.entries.remove(&oldest);
                    }
                }
                let entry = Entry::new(level, message);
                if shown {
                    self.page.entries.append(&entry.widget());
                }
                tool.log.push_back(entry);
            }
        }
        self.refresh(index);
    }

    /// Logs `message` from `tool`, if it says anything, as it starts afresh,
    /// so that past errors no longer show on its icon.
    pub fn started(&self, tool: &str, message: &str) {
        if message.trim().is_empty() {
            return;
        }
        let index = self.tool(tool);
        self.tools.borrow_mut()[index].failed = false;
        self.log(tool, Level::Info, message);
    }

    /// Shows `tool` as busy with `title` while the result lives.
    pub fn busy(self: &Rc<Self>, tool: &str, title: &str) -> Busy {
        let index = self.tool(tool);
        let token = NumberOrString::Number(self.next_token.get());
        self.next_token.set(self.next_token.get().wrapping_add(1));
        self.tools.borrow_mut()[index].works.push(Work {
            client: NO_CLIENT,
            token: token.clone(),
            title: title.to_owned(),
            message: None,
            percentage: None,
        });
        self.settle(index);
        Busy {
            status: Rc::downgrade(self),
            tool: index,
            token,
        }
    }

    /// Follows the progress that `tool`, run by the client numbered `client`,
    /// reports in `params`.
    pub fn update(self: &Rc<Self>, client: usize, tool: &str, params: ProgressParams) {
        let ProgressParamsValue::WorkDone(progress) = params.value;
        let index = self.tool(tool);
        {
            let mut tools = self.tools.borrow_mut();
            let works = &mut tools[index].works;
            let position = works
                .iter()
                .position(|work| work.client == client && work.token == params.token);
            match (progress, position) {
                (WorkDoneProgress::Begin(begin), _) => works.push(Work {
                    client,
                    token: params.token,
                    title: begin.title,
                    message: begin.message,
                    percentage: begin.percentage,
                }),
                (WorkDoneProgress::Report(report), Some(position)) => {
                    let work = &mut works[position];
                    work.message = report.message.or(work.message.take());
                    work.percentage = report.percentage.or(work.percentage);
                }
                (WorkDoneProgress::End(_), Some(position)) => {
                    works.remove(position);
                }
                _ => return,
            }
        }
        self.settle(index);
    }

    /// Forgets the work of the client numbered `client`, whose server
    /// stopped.
    pub fn forget(self: &Rc<Self>, client: usize) {
        let ended: Vec<usize> = self
            .tools
            .borrow_mut()
            .iter_mut()
            .enumerate()
            .filter_map(|(index, tool)| {
                let before = tool.works.len();
                tool.works.retain(|work| work.client != client);
                (tool.works.len() < before).then_some(index)
            })
            .collect();
        for index in ended {
            self.settle(index);
        }
    }

    /// The index of `name`'s tool, which comes up now if it is new.
    fn tool(&self, name: &str) -> usize {
        if let Some(index) = self
            .tools
            .borrow()
            .iter()
            .position(|tool| tool.name == name)
        {
            return index;
        }
        let icon = Icon::new();
        let row = adw::ActionRow::builder()
            .title(name)
            .use_markup(false)
            .subtitle_lines(1)
            .activatable(true)
            .build();
        row.add_suffix(&icon.stack);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        self.list.append(&row);
        self.button.set_visible(true);
        let mut tools = self.tools.borrow_mut();
        tools.push(Tool {
            name: name.to_owned(),
            row,
            icon,
            works: Vec::new(),
            busy: false,
            failed: false,
            log: VecDeque::new(),
        });
        tools.len() - 1
    }

    /// Shows the tool numbered `index` as busy while it has work, and for a
    /// moment after, in case more follows.
    fn settle(self: &Rc<Self>, index: usize) {
        {
            let mut tools = self.tools.borrow_mut();
            let tool = &mut tools[index];
            if !tool.works.is_empty() {
                tool.busy = true;
            } else if tool.busy {
                let this = Rc::downgrade(self);
                glib::timeout_add_local_once(LINGER, move || {
                    let Some(this) = this.upgrade() else {
                        return;
                    };
                    let mut tools = this.tools.borrow_mut();
                    let tool = &mut tools[index];
                    if tool.works.is_empty() {
                        tool.busy = false;
                        drop(tools);
                        this.refresh(index);
                    }
                });
            }
        }
        self.refresh(index);
    }

    /// Shows how the tool numbered `index` is, on its row, its log if it
    /// shows, and the button.
    fn refresh(&self, index: usize) {
        let tools = self.tools.borrow();
        let tool = &tools[index];
        tool.icon.set(tool.state());
        tool.row.set_subtitle(&tool.summary());
        if self.shown.get() == Some(index) {
            self.show_work(tool);
        }
        let states = tools.iter().map(Tool::state);
        self.icon.set(states.max().unwrap_or(State::Idle));
    }

    /// Shows the log of the tool numbered `index`.
    fn open(&self, index: usize) {
        let tools = self.tools.borrow();
        let tool = &tools[index];
        self.shown.set(Some(index));
        self.page.title.set_text(&tool.name);
        self.show_work(tool);
        while let Some(entry) = self.page.entries.first_child() {
            self.page.entries.remove(&entry);
        }
        for entry in &tool.log {
            self.page.entries.append(&entry.widget());
        }
        self.pages.set_visible_child_name("log");
        // Rather than an entry, which would select all it says.
        self.page.back.grab_focus();
    }

    /// Goes back from a log to the list of tools.
    fn close(&self, transition: gtk::StackTransitionType) {
        self.shown.set(None);
        self.pages.set_visible_child_full("tools", transition);
    }

    /// Empties the log that shows, and forgets the errors in it.
    fn clear(&self) {
        let Some(index) = self.shown.get() else {
            return;
        };
        {
            let mut tools = self.tools.borrow_mut();
            tools[index].log.clear();
            tools[index].failed = false;
        }
        while let Some(entry) = self.page.entries.first_child() {
            self.page.entries.remove(&entry);
        }
        self.refresh(index);
    }

    fn show_work(&self, tool: &Tool) {
        let details: Vec<String> = tool.works.iter().map(Work::details).collect();
        self.page.work.set_text(&details.join("\n"));
        self.page.work.set_visible(!details.is_empty());
    }

    /// Ends the work that `busy` stood for.
    fn end(self: &Rc<Self>, busy: &Busy) {
        self.tools.borrow_mut()[busy.tool]
            .works
            .retain(|work| work.client != NO_CLIENT || work.token != busy.token);
        self.settle(busy.tool);
    }
}

impl LogPage {
    fn new() -> Self {
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back")
            .css_classes(["flat"])
            .build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let clear = gtk::Button::builder()
            .label("_Clear")
            .use_underline(true)
            .build();
        let header = gtk::Box::builder().spacing(6).build();
        header.append(&back);
        header.append(&title);
        header.append(&clear);
        let work = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .css_classes(["dim-label"])
            .build();
        let entries = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        root.append(&header);
        root.append(&work);
        root.append(
            &gtk::ScrolledWindow::builder()
                .child(&entries)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(400)
                .build(),
        );
        Self {
            root,
            back,
            title,
            clear,
            work,
            entries,
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(status) = self.status.upgrade() {
            status.end(self);
        }
    }
}

impl Tool {
    fn state(&self) -> State {
        if self.failed {
            State::Failed
        } else if self.busy {
            State::Busy
        } else {
            State::Idle
        }
    }

    /// What the tool is doing, or else the first line of what it last said.
    fn summary(&self) -> String {
        if let Some(work) = self.works.first() {
            return work.summary();
        }
        self.log
            .back()
            .and_then(|entry| entry.message.lines().next())
            .unwrap_or_default()
            .to_owned()
    }
}

impl Work {
    /// What the work is, and how far along if the tool says.
    fn summary(&self) -> String {
        match self.percentage {
            Some(percentage) => format!("{} {percentage}%", self.title),
            None => self.title.clone(),
        }
    }

    /// The summary, with the tool's latest message about the work.
    fn details(&self) -> String {
        match &self.message {
            Some(message) => format!("{} – {message}", self.summary()),
            None => self.summary(),
        }
    }
}

impl Entry {
    fn new(level: Level, message: &str) -> Self {
        let time = glib::DateTime::now_local()
            .and_then(|now| now.format("%H:%M:%S"))
            .map(String::from)
            .unwrap_or_default();
        Self {
            level,
            time,
            message: message.to_owned(),
        }
    }

    /// The entry as shown: its time, how much it matters if much, and what it
    /// says.
    fn widget(&self) -> gtk::Box {
        let heading = gtk::Box::builder().spacing(6).build();
        heading.append(
            &gtk::Label::builder()
                .label(&self.time)
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        let level = match self.level {
            Level::Error => Some(("Error", "error")),
            Level::Warning => Some(("Warning", "warning")),
            Level::Info => None,
        };
        if let Some((label, class)) = level {
            heading.append(
                &gtk::Label::builder()
                    .label(label)
                    .css_classes(["caption-heading", class])
                    .build(),
            );
        }
        let entry = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        entry.append(&heading);
        entry.append(
            &gtk::Label::builder()
                .label(&self.message)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .max_width_chars(50)
                .selectable(true)
                .build(),
        );
        entry
    }
}

impl Icon {
    fn new() -> Self {
        let spinner = gtk::Spinner::new();
        let stack = gtk::Stack::new();
        stack.add_named(
            &gtk::Image::from_icon_name("object-select-symbolic"),
            Some("idle"),
        );
        stack.add_named(&spinner, Some("busy"));
        stack.add_named(
            &gtk::Image::builder()
                .icon_name("dialog-warning-symbolic")
                .css_classes(["warning"])
                .build(),
            Some("failed"),
        );
        Self { stack, spinner }
    }

    fn set(&self, state: State) {
        self.stack.set_visible_child_name(match state {
            State::Idle => "idle",
            State::Busy => "busy",
            State::Failed => "failed",
        });
        self.spinner.set_spinning(state == State::Busy);
    }
}
