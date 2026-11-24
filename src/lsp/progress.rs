//! The header bar's indicator of what language servers are busy with, which
//! shows work once it has gone on for `DELAY`, and stays while work follows
//! work.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;
use gtk::prelude::*;
use lsp_types::{NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress};

/// How long work goes on before it is shown: for now, no time at all.
const DELAY: Duration = Duration::ZERO;

/// How long the indicator stays after the last work ends, in case more
/// begins, as it does between the stages of a server's start.
const LINGER: Duration = Duration::from_millis(500);

pub struct Progress {
    revealer: gtk::Revealer,
    spinner: gtk::Spinner,
    label: gtk::Label,
    /// Work under way, in the order it began.
    works: RefCell<Vec<Work>>,
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

impl Progress {
    pub fn new() -> Rc<Self> {
        let spinner = gtk::Spinner::new();
        let label = gtk::Label::builder().css_classes(["dim-label"]).build();
        let content = gtk::Box::builder().spacing(6).build();
        content.append(&spinner);
        content.append(&label);
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .child(&content)
            .build();
        Rc::new(Self {
            revealer,
            spinner,
            label,
            works: RefCell::default(),
        })
    }

    pub fn widget(&self) -> &gtk::Revealer {
        &self.revealer
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
    pub fn clear(self: &Rc<Self>, client: usize) {
        self.works.borrow_mut().retain(|work| work.client != client);
        self.refresh();
    }

    /// Shows the longest running work, if it has gone on long enough or the
    /// indicator is already up, with the rest in the tooltip. Without work,
    /// hides the indicator once it has lingered.
    fn refresh(self: &Rc<Self>) {
        let showing = self.revealer.reveals_child();
        let works = self.works.borrow();
        let shown: Vec<&Work> = works
            .iter()
            .filter(|work| showing || work.began.elapsed() >= DELAY)
            .collect();
        if let Some(work) = shown.first() {
            self.label.set_text(&work.summary());
            let details: Vec<String> = shown.iter().map(|work| work.details()).collect();
            self.revealer.set_tooltip_text(Some(&details.join("\n")));
            self.spinner.set_spinning(true);
            self.revealer.set_reveal_child(true);
        } else if showing && works.is_empty() {
            let this = Rc::downgrade(self);
            glib::timeout_add_local_once(LINGER, move || {
                if let Some(this) = this.upgrade()
                    && this.works.borrow().is_empty()
                {
                    this.spinner.set_spinning(false);
                    this.revealer.set_reveal_child(false);
                }
            });
        }
    }

    fn refresh_after(self: &Rc<Self>, delay: Duration) {
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            if let Some(this) = this.upgrade() {
                this.refresh();
            }
        });
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
