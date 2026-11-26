//! The header bar's button for the tools Craft runs for a window: language
//! servers, and direnv. Its icon shows how they are, all told: a spinner
//! while any is at work, otherwise a warning sign if any has failed since it
//! last started, and otherwise a tick. It opens onto a list of the tools,
//! each showing the same along with its work, from which each tool's log
//! opens.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib, pango};

/// How long a tool shows as at work after its work ends, in case more
/// begins, as it does between the stages of a server's start.
const LINGER: Duration = Duration::from_millis(500);

/// How many entries each tool's log keeps, dropping the oldest.
const LOG_LENGTH: u32 = 100;

/// The popover's width, which stays put as its pages and what they show
/// change.
const WIDTH: i32 = 400;

/// How much an entry in a log matters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
}

/// How a tool is, or how the tools are, all told, each state showing over
/// those before it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Idle,
    Failed,
    Busy,
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
    /// Numbers work, so that each [`Busy`] knows its own.
    next_work: Cell<u64>,
}

struct Tool {
    name: String,
    row: adw::ActionRow,
    icon: Icon,
    /// Work under way, in the order it began.
    works: Vec<Work>,
    /// Pending while the tool shows as at work for a moment after its work.
    linger: Option<glib::SourceId>,
    /// Whether an error has been logged since the tool last started afresh.
    failed: bool,
    /// What the tool said, as [`Entry`]s, oldest first.
    log: gio::ListStore,
}

/// Work that a tool reports progress on.
struct Work {
    id: u64,
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
/// the tool's name.
struct LogPage {
    root: gtk::Box,
    back: gtk::Button,
    title: gtk::Label,
    /// The tool's work under way, a line each.
    work: gtk::Label,
    /// Bound to the log that shows.
    entries: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    /// Whether the log keeps its latest entry in view, as it does until
    /// scrolled away from it.
    following: Rc<Cell<bool>>,
    /// Shown while the tool has neither work nor a log to show.
    quiet: gtk::Label,
}

/// A tick, a spinner or a warning sign, showing a state.
struct Icon {
    stack: gtk::Stack,
    spinner: gtk::Spinner,
}

/// Work that a tool does while this lives, showing it as at work.
pub struct Busy {
    status: Weak<Status>,
    tool: usize,
    work: u64,
}

impl Status {
    pub fn new() -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();

        let pages = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::SlideLeftRight)
            .vhomogeneous(false)
            .width_request(WIDTH)
            .build();
        let page = LogPage::new();
        pages.add_named(&list, Some("tools"));
        pages.add_named(&page.root, Some("log"));
        let popover = gtk::Popover::builder()
            .child(
                &adw::Clamp::builder()
                    .maximum_size(WIDTH)
                    .tightening_threshold(WIDTH)
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
            next_work: Cell::new(0),
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

    /// Whether the tool named `tool` has come up.
    pub fn knows(&self, tool: &str) -> bool {
        self.tools.borrow().iter().any(|known| known.name == tool)
    }

    /// Logs `message` from `tool`, unless it says nothing, or what the tool
    /// last said. An error shows on the tool's icon until it starts afresh.
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
            let repeat = tool.last().is_some_and(|last| {
                let last = last.borrow::<Entry>();
                last.level == level && last.message == message
            });
            if !repeat {
                if tool.log.n_items() == LOG_LENGTH {
                    tool.log.remove(0);
                }
                let entry = Entry::new(level, message);
                tool.log.append(&glib::BoxedAnyObject::new(entry));
            }
        }
        self.refresh(index);
    }

    /// Logs `message` from `tool`, if it says anything, as the tool starts
    /// afresh, so that past errors no longer show on its icon.
    pub fn started(&self, tool: &str, message: &str) {
        if message.trim().is_empty() {
            return;
        }
        let index = self.tool(tool);
        self.tools.borrow_mut()[index].failed = false;
        self.log(tool, Level::Info, message);
    }

    /// Shows `tool` at work on `title` while the result lives.
    pub fn busy(self: &Rc<Self>, tool: &str, title: &str) -> Busy {
        let index = self.tool(tool);
        let work = self.next_work.get();
        self.next_work.set(work + 1);
        {
            let mut tools = self.tools.borrow_mut();
            let tool = &mut tools[index];
            if let Some(linger) = tool.linger.take() {
                linger.remove();
            }
            tool.works.push(Work {
                id: work,
                title: title.to_owned(),
                message: None,
                percentage: None,
            });
        }
        self.refresh(index);
        Busy {
            status: Rc::downgrade(self),
            tool: index,
            work,
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
            linger: None,
            failed: false,
            log: gio::ListStore::new::<glib::BoxedAnyObject>(),
        });
        tools.len() - 1
    }

    /// Shows how the tool numbered `index` is, on its row, its log page if
    /// that shows, and the button.
    fn refresh(&self, index: usize) {
        let tools = self.tools.borrow();
        let tool = &tools[index];
        tool.icon.set(tool.state());
        tool.row.set_subtitle(&tool.work());
        if self.shown.get() == Some(index) {
            self.show_work(tool);
        }
        let states = tools.iter().map(Tool::state);
        self.icon.set(states.max().unwrap_or(State::Idle));
    }

    /// Shows the log of the tool numbered `index`, with its latest entry in
    /// view.
    fn open(&self, index: usize) {
        let tools = self.tools.borrow();
        let tool = &tools[index];
        self.shown.set(Some(index));
        self.page.title.set_text(&tool.name);
        self.page.following.set(true);
        self.page.entries.bind_model(Some(&tool.log), |item| {
            let entry = item.downcast_ref::<glib::BoxedAnyObject>();
            let entry = entry.expect("logs hold entries").borrow::<Entry>();
            entry.widget().upcast()
        });
        self.show_work(tool);
        self.pages.set_visible_child_name("log");
        // Rather than an entry, which would select all it says.
        self.page.back.grab_focus();
    }

    /// Goes back from a log to the list of tools.
    fn close(&self, transition: gtk::StackTransitionType) {
        self.shown.set(None);
        self.pages.set_visible_child_full("tools", transition);
    }

    /// Shows the work of `tool`, whose log shows, or that it has nothing to
    /// show.
    fn show_work(&self, tool: &Tool) {
        let work = tool.work();
        let said = tool.log.n_items() > 0;
        self.page.work.set_text(&work);
        self.page.work.set_visible(!work.is_empty());
        self.page.scroller.set_visible(said);
        self.page.quiet.set_visible(work.is_empty() && !said);
    }

    /// Ends the work that `busy` stood for, the tool showing as at work a
    /// moment longer if that was the last of its work.
    fn end(self: &Rc<Self>, busy: &Busy) {
        {
            let mut tools = self.tools.borrow_mut();
            let tool = &mut tools[busy.tool];
            tool.works.retain(|work| work.id != busy.work);
            if tool.works.is_empty() {
                let (status, index) = (Rc::downgrade(self), busy.tool);
                let linger = glib::timeout_add_local_once(LINGER, move || {
                    if let Some(status) = status.upgrade() {
                        status.tools.borrow_mut()[index].linger = None;
                        status.refresh(index);
                    }
                });
                if let Some(earlier) = tool.linger.replace(linger) {
                    earlier.remove();
                }
            }
        }
        self.refresh(busy.tool);
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
        let header = gtk::Box::builder().spacing(6).build();
        header.append(&back);
        header.append(&title);
        let work = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .css_classes(["dim-label"])
            .build();
        let entries = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        // A line between each entry and the next.
        entries.set_header_func(|row, before| {
            let separator = before.map(|_| gtk::Separator::new(gtk::Orientation::Horizontal));
            row.set_header(separator.as_ref());
        });
        let scroller = gtk::ScrolledWindow::builder()
            .child(&entries)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(400)
            .build();
        let following = Rc::new(Cell::new(true));
        let adjustment = scroller.vadjustment();
        adjustment.connect_value_changed(glib::clone!(
            #[strong]
            following,
            move |adjustment| following.set(at_end(adjustment))
        ));
        adjustment.connect_changed(glib::clone!(
            #[strong]
            following,
            move |adjustment| {
                if following.get() {
                    // Once laid out: a value set while the log is being laid
                    // out may not move it.
                    let adjustment = adjustment.clone();
                    glib::idle_add_local_once(move || {
                        adjustment.set_value(adjustment.upper() - adjustment.page_size());
                    });
                }
            }
        ));
        let quiet = gtk::Label::builder()
            .label("Nothing to show")
            .css_classes(["dim-label"])
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        root.append(&header);
        root.append(&work);
        root.append(&scroller);
        root.append(&quiet);
        Self {
            root,
            back,
            title,
            work,
            entries,
            scroller,
            following,
            quiet,
        }
    }
}

impl Busy {
    /// Updates what the work says of itself and how far along it is, where
    /// given.
    pub fn report(&self, message: Option<String>, percentage: Option<u32>) {
        let Some(status) = self.status.upgrade() else {
            return;
        };
        if let Some(work) = status.tools.borrow_mut()[self.tool]
            .works
            .iter_mut()
            .find(|work| work.id == self.work)
        {
            work.message = message.or(work.message.take());
            work.percentage = percentage.or(work.percentage);
        }
        status.refresh(self.tool);
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
        if !self.works.is_empty() || self.linger.is_some() {
            State::Busy
        } else if self.failed {
            State::Failed
        } else {
            State::Idle
        }
    }

    /// The tool's work under way, a line each.
    fn work(&self) -> String {
        let works: Vec<String> = self.works.iter().map(Work::describe).collect();
        works.join("\n")
    }

    /// The latest entry in the tool's log.
    fn last(&self) -> Option<glib::BoxedAnyObject> {
        self.log
            .item(self.log.n_items().checked_sub(1)?)
            .and_downcast()
    }
}

impl Work {
    /// What the work is, how far along, and its latest message, where the
    /// tool says.
    fn describe(&self) -> String {
        let percentage = (self.percentage)
            .map(|percentage| format!(" {percentage}%"))
            .unwrap_or_default();
        let message = (self.message.as_ref())
            .map(|message| format!(" – {message}"))
            .unwrap_or_default();
        format!("{}{percentage}{message}", self.title)
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
    fn widget(&self) -> gtk::ListBoxRow {
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
            .margin_top(6)
            .margin_bottom(6)
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
        gtk::ListBoxRow::builder()
            .child(&entry)
            .activatable(false)
            .build()
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

/// Whether `adjustment` is scrolled to the end.
fn at_end(adjustment: &gtk::Adjustment) -> bool {
    adjustment.value() >= adjustment.upper() - adjustment.page_size()
}
