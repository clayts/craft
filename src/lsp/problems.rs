//! The header bar's list of what went wrong with language servers: a button
//! that shows while there are problems, and lists them in full.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::pango;
use gtk::prelude::*;

pub struct Problems {
    button: gtk::MenuButton,
    list: gtk::Box,
    /// Each problem listed, by source and message, to list each once.
    listed: RefCell<Vec<(String, String)>>,
}

impl Problems {
    pub fn new() -> Rc<Self> {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        let clear = gtk::Button::builder()
            .label("_Clear")
            .use_underline(true)
            .halign(gtk::Align::End)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        // First, so that it rather than a message takes the focus.
        content.append(&clear);
        content.append(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_width(true)
                .propagate_natural_height(true)
                .max_content_height(400)
                .build(),
        );
        let button = gtk::MenuButton::builder()
            .icon_name("dialog-warning-symbolic")
            .tooltip_text("Language Server Problems")
            .css_classes(["warning"])
            .popover(&gtk::Popover::builder().child(&content).build())
            .visible(false)
            .build();
        let problems = Rc::new(Self {
            button,
            list,
            listed: RefCell::default(),
        });
        let weak = Rc::downgrade(&problems);
        clear.connect_clicked(move |_| {
            if let Some(problems) = weak.upgrade() {
                problems.clear();
            }
        });
        problems
    }

    pub fn widget(&self) -> &gtk::MenuButton {
        &self.button
    }

    /// Lists `message` from `source`, such as a server, unless it is listed
    /// already.
    pub fn report(&self, source: &str, message: &str) {
        let problem = (source.to_owned(), message.trim().to_owned());
        if self.listed.borrow().contains(&problem) {
            return;
        }
        let entry = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        entry.append(
            &gtk::Label::builder()
                .label(&problem.0)
                .xalign(0.0)
                .css_classes(["heading"])
                .build(),
        );
        entry.append(
            &gtk::Label::builder()
                .label(&problem.1)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .max_width_chars(60)
                .selectable(true)
                .build(),
        );
        self.list.append(&entry);
        self.listed.borrow_mut().push(problem);
        self.button.set_visible(true);
    }

    /// Forgets every problem, hiding the button.
    pub fn clear(&self) {
        self.button.popdown();
        self.button.set_visible(false);
        while let Some(entry) = self.list.first_child() {
            self.list.remove(&entry);
        }
        self.listed.borrow_mut().clear();
    }
}
