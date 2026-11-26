//! The header bar's button for the tools Craft runs for a window: language
//! servers, and direnv. Its icon shows how they are, all told: a spinner
//! while any is at work, otherwise a warning sign if any has failed since it
//! last started, and otherwise a tick. It opens onto a list of the tools,
//! each showing the same along with its work, from which each tool's log
//! opens in a tab.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::editor::follow_style;

/// The window action that opens a tool's log, given the tool's name.
pub const SHOW_LOG: &str = "win.show-log";

/// How long a tool shows as at work after its work ends, in case more
/// begins, as it does between the stages of a server's start.
const LINGER: Duration = Duration::from_millis(500);

/// How many lines each log keeps, dropping its oldest entries.
const LOG_LINES: i32 = 1000;

/// The popover's width, which stays put as the work it shows changes.
const WIDTH: i32 = 400;

/// Indents the lines that continue a log entry, past its time.
const INDENT: &str = "         ";

/// How much a log entry matters.
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
    list: gtk::ListBox,
    /// The tools, in the order they first came up, as their rows are.
    tools: RefCell<Vec<Tool>>,
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
    /// What the tool last said, which it is not to repeat.
    last: Option<(Level, String)>,
    /// What the tool said, as text to read like a file.
    log: sourceview::Buffer,
}

/// Work that a tool reports progress on.
struct Work {
    id: u64,
    title: String,
    message: Option<String>,
    percentage: Option<u32>,
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
            .width_request(WIDTH)
            .build();
        let popover = gtk::Popover::builder()
            .child(
                &adw::Clamp::builder()
                    .maximum_size(WIDTH)
                    .tightening_threshold(WIDTH)
                    .child(&list)
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
        Rc::new(Self {
            button,
            icon,
            list,
            tools: RefCell::default(),
            next_work: Cell::new(0),
        })
    }

    pub fn widget(&self) -> &gtk::MenuButton {
        &self.button
    }

    /// The log of the tool named `tool`, if it has come up.
    pub fn log_of(&self, tool: &str) -> Option<sourceview::Buffer> {
        let tools = self.tools.borrow();
        let tool = tools.iter().find(|known| known.name == tool)?;
        Some(tool.log.clone())
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
            let repeat =
                (tool.last.as_ref()).is_some_and(|(last, said)| *last == level && said == message);
            if !repeat {
                append(&tool.log, &entry(level, message));
                tool.last = Some((level, message.to_owned()));
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
            .action_name(SHOW_LOG)
            .action_target(&name.to_variant())
            .build();
        row.add_suffix(&icon.stack);
        self.list.append(&row);
        self.button.set_visible(true);
        let log = sourceview::Buffer::new(None);
        log.set_enable_undo(false);
        follow_style(&log);
        let mut tools = self.tools.borrow_mut();
        tools.push(Tool {
            name: name.to_owned(),
            row,
            icon,
            works: Vec::new(),
            linger: None,
            failed: false,
            last: None,
            log,
        });
        tools.len() - 1
    }

    /// Shows how the tool numbered `index` is, on its row and the button.
    fn refresh(&self, index: usize) {
        let tools = self.tools.borrow();
        let tool = &tools[index];
        tool.icon.set(tool.state());
        let works: Vec<String> = tool.works.iter().map(Work::describe).collect();
        tool.row.set_subtitle(&works.join("\n"));
        let states = tools.iter().map(Tool::state);
        self.icon.set(states.max().unwrap_or(State::Idle));
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

/// `message` as a log entry: a line with the time, and how much it matters
/// if much, then the message's further lines, indented.
fn entry(level: Level, message: &str) -> String {
    let time = glib::DateTime::now_local()
        .and_then(|now| now.format("%H:%M:%S"))
        .map_or_else(|_| "--:--:--".to_owned(), String::from);
    let level = match level {
        Level::Error => "error: ",
        Level::Warning => "warning: ",
        Level::Info => "",
    };
    let lines: Vec<&str> = message
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    format!("{time} {level}{}\n", lines.join(&format!("\n{INDENT}")))
}

/// Adds `entry` to the end of `log`, dropping the oldest entries beyond
/// [`LOG_LINES`].
fn append(log: &sourceview::Buffer, entry: &str) {
    log.insert(&mut log.end_iter(), entry);
    let excess = log.line_count() - LOG_LINES;
    if excess > 0
        && let Some(mut end) = log.iter_at_line(excess)
    {
        // On to the start of an entry, rather than into one, keeping the
        // latest whole however long.
        while end.char() == ' ' {
            end.forward_line();
        }
        if !end.is_end() {
            log.delete(&mut log.start_iter(), &mut end);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_indent_their_further_lines() {
        let error = entry(Level::Error, "Failed\n\nbecause  \n  of this");
        let (time, rest) = error.split_once(' ').unwrap();
        assert_eq!(time.len() + 1, INDENT.len());
        assert_eq!(
            rest,
            format!("error: Failed\n{INDENT}because\n{INDENT}  of this\n")
        );
        assert!(entry(Level::Info, "Started").ends_with(" Started\n"));
    }
}
