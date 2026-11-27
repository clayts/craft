//! What the tools Craft runs for a window, its language servers and direnv,
//! are doing, and what has gone wrong, as the header bar shows: a spinner
//! while any is at work, its tooltip saying on what, and a warning sign
//! while any has failed since it last started, its tooltip saying how.
//! Everything they say goes to the standard error as well, as a log.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;

/// How much something a tool says matters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
}

pub struct Status {
    widget: gtk::Box,
    spinner: adw::Spinner,
    alert: gtk::Image,
    /// Work under way, in the order it began.
    works: RefCell<Vec<Work>>,
    /// The errors of each tool since it last started afresh, with the tool's
    /// name, in the order they came.
    errors: RefCell<Vec<(String, String)>>,
    /// Numbers work, so that each [`Busy`] knows its own.
    next_work: Cell<u64>,
}

/// Work that a tool reports progress on.
struct Work {
    id: u64,
    tool: String,
    title: String,
    message: Option<String>,
    percentage: Option<u32>,
}

/// Work that a tool does while this lives.
pub struct Busy {
    status: Weak<Status>,
    work: u64,
}

impl Status {
    pub fn new() -> Rc<Self> {
        let spinner = adw::Spinner::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let alert = gtk::Image::builder()
            .icon_name("dialog-warning-symbolic")
            .css_classes(["warning"])
            .visible(false)
            .build();
        let widget = gtk::Box::builder().spacing(12).visible(false).build();
        widget.append(&spinner);
        widget.append(&alert);
        Rc::new(Self {
            widget,
            spinner,
            alert,
            works: RefCell::default(),
            errors: RefCell::default(),
            next_work: Cell::new(0),
        })
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    /// Logs `message` from `tool`, if it says anything. An error also shows
    /// until the tool starts afresh.
    pub fn log(&self, tool: &str, level: Level, message: &str) {
        let message = message.trim();
        if message.is_empty() {
            return;
        }
        let kind = match level {
            Level::Error => "error: ",
            Level::Warning => "warning: ",
            Level::Info => "",
        };
        eprintln!("{tool}: {kind}{message}");
        if level == Level::Error {
            let mut errors = self.errors.borrow_mut();
            if !(errors.iter()).any(|(own, error)| own == tool && error == message) {
                errors.push((tool.to_owned(), message.to_owned()));
            }
            drop(errors);
            self.refresh();
        }
    }

    /// Forgets `tool`'s errors, as it has started afresh.
    pub fn started(&self, tool: &str) {
        self.errors.borrow_mut().retain(|(own, _)| own != tool);
        self.refresh();
    }

    /// Shows `tool` at work on `title` while the result lives.
    pub fn busy(self: &Rc<Self>, tool: &str, title: &str) -> Busy {
        let work = self.next_work.get();
        self.next_work.set(work + 1);
        self.works.borrow_mut().push(Work {
            id: work,
            tool: tool.to_owned(),
            title: title.to_owned(),
            message: None,
            percentage: None,
        });
        self.refresh();
        Busy {
            status: Rc::downgrade(self),
            work,
        }
    }

    /// Shows the spinner while there is work, and the warning sign while
    /// there are errors, each with its tooltip saying what.
    fn refresh(&self) {
        let works = self.works.borrow();
        let doing: Vec<String> = works.iter().map(Work::describe).collect();
        self.spinner.set_tooltip_text(Some(&doing.join("\n")));
        self.spinner.set_visible(!works.is_empty());
        let errors = self.errors.borrow();
        let failed: Vec<String> = (errors.iter())
            .map(|(tool, error)| format!("{tool}: {error}"))
            .collect();
        self.alert.set_tooltip_text(Some(&failed.join("\n\n")));
        self.alert.set_visible(!errors.is_empty());
        self.widget
            .set_visible(!works.is_empty() || !errors.is_empty());
    }

    /// Ends the work that `busy` stood for.
    fn end(&self, busy: &Busy) {
        self.works.borrow_mut().retain(|work| work.id != busy.work);
        self.refresh();
    }
}

impl Busy {
    /// Updates what the work says of itself and how far along it is, where
    /// given.
    pub fn report(&self, message: Option<String>, percentage: Option<u32>) {
        let Some(status) = self.status.upgrade() else {
            return;
        };
        if let Some(work) = (status.works.borrow_mut().iter_mut()).find(|work| work.id == self.work)
        {
            work.message = message.or(work.message.take());
            work.percentage = percentage.or(work.percentage);
        }
        status.refresh();
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(status) = self.status.upgrade() {
            status.end(self);
        }
    }
}

impl Work {
    /// What tool is doing the work, what it is, how far along, and its latest
    /// message, where the tool says.
    fn describe(&self) -> String {
        let percentage = (self.percentage)
            .map(|percentage| format!(" {percentage}%"))
            .unwrap_or_default();
        let message = (self.message.as_ref())
            .map(|message| format!(" – {message}"))
            .unwrap_or_default();
        format!("{}: {}{percentage}{message}", self.tool, self.title)
    }
}
