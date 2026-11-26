//! Language server support: a window runs a server per language, for its
//! project folder if it shows one, found on the project's `PATH`, and connects
//! each open file to its server.

mod client;
mod completion;
mod document;
mod environment;
mod hover;
mod position;
mod semantic;
mod status;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::rc::Rc;

use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, Shared};
use gtk::prelude::*;
use gtk::{gio, glib};

use client::Client;
pub use document::Document;
use environment::Environment;
pub use position::Encoding;
use position::Shift;
pub use status::SHOW_LOG;
use status::{Level, Status};

use crate::editor::Editor;
use crate::theme;

/// How the status names the theme, when it has something to say of it.
const THEME: &str = "theme.json";

/// A project's environment, loaded once for all of its servers.
type SharedEnvironment = Shared<LocalBoxFuture<'static, Rc<Environment>>>;

/// A language server: the command lines to try, the first found on the
/// project's `PATH` winning, and the `GtkSourceView` languages it serves with
/// the LSP identifier of each.
struct Server {
    commands: &'static [&'static [&'static str]],
    languages: &'static [(&'static str, &'static str)],
}

const SERVERS: &[Server] = &[
    Server {
        commands: &[&["rust-analyzer"]],
        languages: &[("rust", "rust")],
    },
    Server {
        commands: &[&["clangd"]],
        languages: &[
            ("c", "c"),
            ("chdr", "c"),
            ("cpp", "cpp"),
            ("cpphdr", "cpp"),
            ("objc", "objective-c"),
        ],
    },
    Server {
        commands: &[&["gopls"]],
        languages: &[("go", "go")],
    },
    Server {
        commands: &[
            &["basedpyright-langserver", "--stdio"],
            &["pyright-langserver", "--stdio"],
            &["pylsp"],
        ],
        languages: &[("python3", "python"), ("python", "python")],
    },
    Server {
        commands: &[&["typescript-language-server", "--stdio"]],
        languages: &[
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("typescript", "typescript"),
            ("typescript-jsx", "typescriptreact"),
        ],
    },
    Server {
        commands: &[&["nixd"], &["nil"]],
        languages: &[("nix", "nix")],
    },
    Server {
        commands: &[&["bash-language-server", "start"]],
        languages: &[("sh", "shellscript")],
    },
    Server {
        commands: &[&["zls"]],
        languages: &[("zig", "zig")],
    },
    Server {
        commands: &[&["lua-language-server"]],
        languages: &[("lua", "lua")],
    },
    Server {
        commands: &[&["taplo", "lsp", "stdio"]],
        languages: &[("toml", "toml")],
    },
    Server {
        commands: &[&["vscode-json-language-server", "--stdio"]],
        languages: &[("json", "json")],
    },
    Server {
        commands: &[&["yaml-language-server", "--stdio"]],
        languages: &[("yaml", "yaml")],
    },
    Server {
        commands: &[&["vscode-html-language-server", "--stdio"]],
        languages: &[("html", "html")],
    },
    Server {
        commands: &[&["vscode-css-language-server", "--stdio"]],
        languages: &[("css", "css")],
    },
];

impl Server {
    /// The name of the server's first program, which stands for the server
    /// until one is found.
    fn name(&self) -> &'static str {
        self.commands[0][0]
    }

    /// Says that none of the server's programs is on `path`, and what that
    /// holds.
    fn not_found(&self, path: Option<&OsStr>) -> String {
        let others: Vec<&str> = self.commands[1..]
            .iter()
            .map(|command| command[0])
            .collect();
        let missing = match others.as_slice() {
            [] => "Not found on the PATH".to_owned(),
            [other] => format!("Not found on the PATH, nor is {other}"),
            [rest @ .., last] => {
                format!(
                    "Not found on the PATH, nor are {} or {last}",
                    rest.join(", ")
                )
            }
        };
        let dirs: Vec<String> = path
            .map(|path| {
                std::env::split_paths(path)
                    .map(|dir| dir.display().to_string())
                    .collect()
            })
            .unwrap_or_default();
        format!("{missing}\nThe PATH holds:\n{}", dirs.join("\n"))
    }
}

/// The server for the `GtkSourceView` language `language`, by index, with the
/// language's LSP identifier.
fn server_for(language: &str) -> Option<(usize, &'static str)> {
    SERVERS.iter().enumerate().find_map(|(index, server)| {
        let &(_, id) = server
            .languages
            .iter()
            .find(|&&(name, _)| name == language)?;
        Some((index, id))
    })
}

/// A window's language servers, by server.
pub struct Servers {
    /// The project folder the servers are for, once the window shows one.
    root: RefCell<Option<gio::File>>,
    clients: RefCell<HashMap<usize, Rc<Client>>>,
    /// The environment servers run in, once one needs it.
    environment: RefCell<Option<SharedEnvironment>>,
    status: Rc<Status>,
    /// Reports on the theme as it changes.
    _theme: theme::Watch,
}

impl Default for Servers {
    fn default() -> Self {
        let status = Status::new();
        let report = {
            let status = Rc::downgrade(&status);
            move || {
                if let Some(status) = status.upgrade() {
                    report_theme(&status);
                }
            }
        };
        // As the theme may have failed to load before this window opened.
        report();
        Self {
            root: RefCell::default(),
            clients: RefCell::default(),
            environment: RefCell::default(),
            status,
            _theme: theme::watch(report),
        }
    }
}

/// Tells `status` why `theme.json` is not in use, or that it is now, after
/// not being.
fn report_theme(status: &Status) {
    match theme::error() {
        Some(error) => status.log(THEME, Level::Error, &error),
        None if status.log_of(THEME).is_some() => status.started(THEME, "Loaded"),
        None => {}
    }
}

impl Servers {
    /// Shows how the servers and direnv are, and what they are doing.
    pub fn indicator(&self) -> &gtk::MenuButton {
        self.status.widget()
    }

    /// What the tool named `tool`, a server or direnv, has said.
    pub fn log(&self, tool: &str) -> Option<sourceview::Buffer> {
        self.status.log_of(tool)
    }

    /// Runs the servers for the project folder `root`, which the window now
    /// shows, connecting `editors` to them.
    pub fn set_root(&self, root: &gio::File, editors: impl IntoIterator<Item = Editor>) {
        self.shutdown();
        self.root.replace(Some(root.clone()));
        for editor in editors {
            self.attach(&editor);
        }
    }

    /// Connects `editor` to the server for its language, starting it if
    /// needed.
    pub fn attach(&self, editor: &Editor) {
        let Some((index, language)) = editor.language().and_then(|name| server_for(&name)) else {
            return;
        };
        let server = &SERVERS[index];
        let root = self.root.borrow().clone();
        let running = self
            .clients
            .borrow()
            .get(&index)
            .filter(|client| !client.is_stopped())
            .cloned();
        let client = if let Some(client) = running {
            client
        } else {
            let environment = self.environment();
            // Once the environment is known, a missing server is too.
            if let Some(environment) = environment.peek()
                && environment.find(server.commands).is_none()
            {
                let missing = server.not_found(environment.path().as_deref());
                self.status.log(server.name(), Level::Error, &missing);
                return;
            }
            let client = Client::start(server, root.as_ref(), environment, Rc::clone(&self.status));
            let stopped = self.clients.borrow_mut().insert(index, Rc::clone(&client));
            // The new server takes over the stopped one's files.
            for document in stopped.iter().flat_map(|stopped| stopped.documents()) {
                if let Some(editor) = document.editor() {
                    let document = Document::new(&client, &editor, document.language());
                    editor.set_document(Some(document));
                }
            }
            client
        };
        editor.set_document(Some(Document::new(&client, editor, language)));
    }

    /// The project's environment, loaded once, or again after failing, in
    /// case the cause has been fixed. Without a project, Craft's own.
    fn environment(&self) -> SharedEnvironment {
        if let Some(environment) = &*self.environment.borrow()
            && !environment
                .peek()
                .is_some_and(|environment| environment.failed)
        {
            return environment.clone();
        }
        let dir = self.root.borrow().as_ref().and_then(FileExt::path);
        let status = Rc::clone(&self.status);
        let environment = async move {
            Rc::new(match dir {
                Some(dir) => Environment::load(&dir, &status).await,
                None => Environment::default(),
            })
        }
        .boxed_local()
        .shared();
        self.environment.replace(Some(environment.clone()));
        environment
    }

    /// Shuts every server down, keeping the application running until they
    /// have exited.
    pub fn shutdown(&self) {
        self.environment.take();
        for client in self.clients.take().into_values() {
            let hold = gio::Application::default().map(|application| application.hold());
            glib::spawn_future_local(async move {
                client.shutdown().await;
                drop(hold);
            });
        }
    }
}
