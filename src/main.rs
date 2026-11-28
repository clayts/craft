//! Craft: a minimal code editor.

mod editor;
mod file_tree;
mod hover;
mod lsp;
mod recent;
mod search;
mod theme;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

use window::Window;

const APP_ID: &str = "io.github.clayts.Craft";

fn main() -> glib::ExitCode {
    // Each run of Craft is its own, with its own windows and the environment
    // it was started in.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(startup);
    app.connect_activate(|app| Window::new(app).present());
    app.connect_open(|app, files, _hint| open(app, files));
    app.run()
}

fn startup(app: &adw::Application) {
    sourceview::init();
    let style = gtk::CssProvider::new();
    style.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &style,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    // Closing each window, rather than quitting outright, lets windows with
    // unsaved changes ask first.
    let quit = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.windows().iter().for_each(gtk::Window::close))
        .build();
    app.add_action_entries([quit]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
}

/// Opens each directory as a project in a window of its own, then the files
/// as tabs, in order, in the last of those windows, or else in a window of
/// their own.
fn open(app: &adw::Application, files: &[gio::File]) {
    let (dirs, files): (Vec<_>, Vec<_>) = files.iter().cloned().partition(|file| {
        file.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
            == gio::FileType::Directory
    });
    let mut last = None;
    for dir in &dirs {
        let window = Window::new(app);
        window.set_project(dir);
        window.present();
        last = Some(window);
    }
    if !files.is_empty() {
        let window = last.unwrap_or_else(|| Window::new(app));
        window.open_files(files);
        window.present();
    }
}

/// The name of `file` as shown to the user.
fn display_name(file: &gio::File) -> String {
    file.basename().map_or_else(
        || file.uri().into(),
        |name| glib::filename_display_name(name).into(),
    )
}
