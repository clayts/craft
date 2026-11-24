//! Craft: a minimal code editor.

mod editor;
mod file_tree;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

use window::Window;

const APP_ID: &str = "io.github.clayts.Craft";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(startup);
    app.connect_activate(|app| active_window(app).present());
    app.connect_open(|app, files, _hint| open(app, files));
    app.run()
}

fn startup(app: &adw::Application) {
    sourceview::init();
    // Closing each window, rather than quitting outright, lets windows with
    // unsaved changes ask first.
    let quit = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.windows().iter().for_each(gtk::Window::close))
        .build();
    app.add_action_entries([quit]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
}

/// Opens each directory as a project, then the files, in order, in the last
/// project's window or else the active one.
fn open(app: &adw::Application, files: &[gio::File]) {
    let (dirs, files): (Vec<_>, Vec<_>) = files.iter().cloned().partition(|file| {
        file.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
            == gio::FileType::Directory
    });
    let mut target = None;
    for dir in &dirs {
        let window = project_window(app, dir);
        window.present();
        target = Some(window);
    }
    if !files.is_empty() {
        let window = target.unwrap_or_else(|| active_window(app));
        window.open_files(files);
        window.present();
    }
}

/// The active window, or a new one if there is none.
fn active_window(app: &adw::Application) -> Window {
    app.active_window()
        .and_downcast()
        .unwrap_or_else(|| Window::new(app))
}

/// The window showing `dir`: an existing one, the active one if it has no
/// project yet, or a new one.
fn project_window(app: &adw::Application, dir: &gio::File) -> Window {
    let mut windows = app
        .windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Window>().ok());
    if let Some(window) = windows.find(|w| w.project().is_some_and(|p| p.equal(dir))) {
        return window;
    }
    let window = app
        .active_window()
        .and_downcast::<Window>()
        .filter(|w| w.project().is_none())
        .unwrap_or_else(|| Window::new(app));
    window.set_project(dir);
    window
}

/// The name of `file` as shown to the user.
fn display_name(file: &gio::File) -> String {
    file.basename().map_or_else(
        || file.uri().into(),
        |name| glib::filename_display_name(name).into(),
    )
}
