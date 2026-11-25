//! The header bar's button for the language servers. Its icon shows whether
//! all is well, servers are busy, or something has gone wrong, and it opens
//! onto their work under way and a log of what they and direnv said.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{glib, pango};
use lsp_types::{NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress};

/// How long work goes on before it is shown: for now, no time at all.
const DELAY: Duration = Duration::ZERO;

/// How long the spinner stays after the last work ends, in case more begins,
/// as it does between the stages of a server's start.
const LINGER: Duration = Duration::from_millis(500);

/// How many entries the log keeps, dropping the oldest.
const LOG_LENGTH: usize = 100;

/// How much an entry in the log matters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
}

pub struct Status {
    button: gtk::MenuButton,
    icons: gtk::Stack,
    spinner: gtk::Spinner,
    /// The work shown, a line each.
    work: gtk::Label,
    log: gtk::Box,
    scroller: gtk::ScrolledWindow,
    clear: gtk::Button,
    /// Shown while there is neither work nor a log to show.
    quiet: gtk::Label,
    /// Work under way, in the order it began.
    works: RefCell<Vec<Work>>,
    /// Whether work shows, which it goes on doing while work follows work.
    busy: Cell<bool>,
    /// Each entry in the log, by source and message, to log each once.
    logged: RefCell<Vec<(String, String)>>,
    /// Whether an error has been logged since the log was last cleared.
    failed: Cell<bool>,
}

/// Work that a server reports progress on.
struct Work {
    client: usize,
    token: NumberOrString,
    server: &'static str,
    title: String,
    message: Option<String>,
    percentage: Option<u32>,
    began: Instant,
}

impl Status {
    pub fn new() -> Rc<Self> {
        let spinner = gtk::Spinner::new();
        let icons = gtk::Stack::new();
        icons.add_named(
            &gtk::Image::from_icon_name("object-select-symbolic"),
            Some("idle"),
        );
        icons.add_named(&spinner, Some("busy"));
        icons.add_named(
            &gtk::Image::builder()
                .icon_name("dialog-warning-symbolic")
                .css_classes(["warning"])
                .build(),
            Some("failed"),
        );

        let clear = gtk::Button::builder()
            .label("_Clear")
            .use_underline(true)
            .halign(gtk::Align::End)
            .build();
        let work = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(60)
            .css_classes(["dim-label"])
            .build();
        let quiet = gtk::Label::builder()
            .label("Nothing to show")
            .css_classes(["dim-label"])
            .build();
        let log = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        // First, so that it rather than an entry takes the focus.
        let scroller = gtk::ScrolledWindow::builder()
            .child(&log)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_width(true)
            .propagate_natural_height(true)
            .max_content_height(400)
            .build();
        content.append(&clear);
        content.append(&work);
        content.append(&quiet);
        content.append(&scroller);
        let button = gtk::MenuButton::builder()
            .child(&icons)
            .popover(&gtk::Popover::builder().child(&content).build())
            .visible(false)
            .build();

        let status = Rc::new(Self {
            button,
            icons,
            spinner,
            work,
            log,
            scroller,
            clear,
            quiet,
            works: RefCell::default(),
            busy: Cell::new(false),
            logged: RefCell::default(),
            failed: Cell::new(false),
        });
        let weak = Rc::downgrade(&status);
        status.clear.connect_clicked(move |_| {
            if let Some(status) = weak.upgrade() {
                status.clear();
            }
        });
        status.refresh();
        status
    }

    pub fn widget(&self) -> &gtk::MenuButton {
        &self.button
    }

    /// Follows the progress that `server`, run by the client numbered
    /// `client`, reports in `params`.
    pub fn update(self: &Rc<Self>, client: usize, server: &'static str, params: ProgressParams) {
        let ProgressParamsValue::WorkDone(progress) = params.value;
        let token = params.token;
        {
            let mut works = self.works.borrow_mut();
            let index = works
                .iter()
                .position(|work| work.client == client && work.token == token);
            match (progress, index) {
                (WorkDoneProgress::Begin(begin), _) => {
                    works.push(Work {
                        client,
                        token,
                        server,
                        title: begin.title,
                        message: begin.message,
                        percentage: begin.percentage,
                        began: Instant::now(),
                    });
                    self.refresh_after(DELAY);
                }
                (WorkDoneProgress::Report(report), Some(index)) => {
                    let work = &mut works[index];
                    work.message = report.message.or(work.message.take());
                    work.percentage = report.percentage.or(work.percentage);
                }
                (WorkDoneProgress::End(_), Some(index)) => {
                    works.remove(index);
                }
                _ => return,
            }
        }
        self.refresh();
    }

    /// Forgets the work of the client numbered `client`, whose server
    /// stopped.
    pub fn forget(self: &Rc<Self>, client: usize) {
        self.works.borrow_mut().retain(|work| work.client != client);
        self.refresh();
    }

    /// Logs `message` from `source`, such as a server, unless it is logged
    /// already.
    pub fn log(&self, level: Level, source: &str, message: &str) {
        if level == Level::Error {
            self.failed.set(true);
        }
        let entry = (source.to_owned(), message.trim().to_owned());
        if !entry.1.is_empty() && !self.logged.borrow().contains(&entry) {
            let mut logged = self.logged.borrow_mut();
            if logged.len() == LOG_LENGTH {
                logged.remove(0);
                if let Some(oldest) = self.log.first_child() {
                    self.log.remove(&oldest);
                }
            }
            self.log.append(&entry_widget(level, &entry.0, &entry.1));
            logged.push(entry);
        }
        self.show();
    }

    /// Empties the log, and forgets that anything went wrong.
    pub fn clear(&self) {
        while let Some(entry) = self.log.first_child() {
            self.log.remove(&entry);
        }
        self.logged.borrow_mut().clear();
        self.failed.set(false);
        self.show();
    }

    /// Shows the work that has gone on long enough, or all of it while work
    /// shows already. Without work, stops showing it once it has lingered.
    fn refresh(self: &Rc<Self>) {
        let details: Vec<String> = self
            .works
            .borrow()
            .iter()
            .filter(|work| self.busy.get() || work.began.elapsed() >= DELAY)
            .map(Work::details)
            .collect();
        if !details.is_empty() {
            self.busy.set(true);
        } else if self.busy.get() && self.works.borrow().is_empty() {
            let this = Rc::downgrade(self);
            glib::timeout_add_local_once(LINGER, move || {
                if let Some(this) = this.upgrade()
                    && this.works.borrow().is_empty()
                {
                    this.busy.set(false);
                    this.show();
                }
            });
        }
        self.work.set_text(&details.join("\n"));
        self.work.set_visible(!details.is_empty());
        self.button.set_tooltip_text(Some(
            details.first().map_or("Language Servers", String::as_str),
        ));
        self.show();
    }

    fn refresh_after(self: &Rc<Self>, delay: Duration) {
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            if let Some(this) = this.upgrade() {
                this.refresh();
            }
        });
    }

    /// Shows on the button whether something went wrong, or else whether
    /// work shows, and in its popover whether there is anything to show.
    fn show(&self) {
        let (icon, spinning) = if self.failed.get() {
            ("failed", false)
        } else if self.busy.get() {
            ("busy", true)
        } else {
            ("idle", false)
        };
        self.icons.set_visible_child_name(icon);
        self.spinner.set_spinning(spinning);
        let logged = !self.logged.borrow().is_empty();
        self.clear.set_sensitive(logged);
        self.scroller.set_visible(logged);
        self.quiet.set_visible(!logged && !self.work.is_visible());
        // Once there has been anything to show, the button stays.
        if logged || self.busy.get() {
            self.button.set_visible(true);
        }
    }
}

impl Work {
    /// What the work is, and how far along if the server says.
    fn summary(&self) -> String {
        match self.percentage {
            Some(percentage) => format!("{} {percentage}%", self.title),
            None => self.title.clone(),
        }
    }

    /// The summary, with the server doing the work and its latest message.
    fn details(&self) -> String {
        let summary = format!("{}: {}", self.server, self.summary());
        match &self.message {
            Some(message) => format!("{summary} – {message}"),
            None => summary,
        }
    }
}

/// An entry in the log: where it came from, at what time, and what it says.
fn entry_widget(level: Level, source: &str, message: &str) -> gtk::Box {
    let name = gtk::Label::builder()
        .label(source)
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["heading"])
        .build();
    match level {
        Level::Error => name.add_css_class("error"),
        Level::Warning => name.add_css_class("warning"),
        Level::Info => {}
    }
    let time = glib::DateTime::now_local()
        .and_then(|now| now.format("%H:%M:%S"))
        .unwrap_or_default();
    let heading = gtk::Box::builder().spacing(12).build();
    heading.append(&name);
    heading.append(
        &gtk::Label::builder()
            .label(time)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    let entry = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    entry.append(&heading);
    entry.append(
        &gtk::Label::builder()
            .label(message)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .max_width_chars(60)
            .selectable(true)
            .build(),
    );
    entry
}
