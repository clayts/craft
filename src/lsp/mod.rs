//! Language server support: a window runs a server per language, for its
//! project folder if it shows one, found on the project's `PATH`, and connects
//! each open file to its server.

mod client;
mod completion;
mod document;
mod environment;
mod hover;
mod position;
mod progress;

use std::cell::RefCell;
use std::collections::HashMap;
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
use progress::Progress;

use crate::editor::Editor;

/// Tells the user of a problem.
type Report = Rc<dyn Fn(&str)>;

/// A project's environment, loaded once for all of its servers.
type SharedEnvironment = Shared<LocalBoxFuture<'static, Rc<Environment>>>;

/// A language server: the command lines to try, the first found on the
/// project's `PATH` winning, and the `GtkSourceView` languages it serves with
/// the LSP identifier of each.
struct Server {
    commands: &'static [&'static [&'static str]],
    languages: &'static [(&'static str, &'static str)],
    /// Whether the server needs a project folder, giving nothing for a file
    /// opened on its own.
    needs_folder: bool,
}

const SERVERS: &[Server] = &[
    Server {
        commands: &[&["rust-analyzer"]],
        languages: &[("rust", "rust")],
        needs_folder: true,
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
        needs_folder: false,
    },
    Server {
        commands: &[&["gopls"]],
        languages: &[("go", "go")],
        needs_folder: false,
    },
    Server {
        commands: &[
            &["basedpyright-langserver", "--stdio"],
            &["pyright-langserver", "--stdio"],
            &["pylsp"],
        ],
        languages: &[("python3", "python"), ("python", "python")],
        needs_folder: false,
    },
    Server {
        commands: &[&["typescript-language-server", "--stdio"]],
        languages: &[
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("typescript", "typescript"),
            ("typescript-jsx", "typescriptreact"),
        ],
        needs_folder: false,
    },
    Server {
        commands: &[&["nixd"], &["nil"]],
        languages: &[("nix", "nix")],
        needs_folder: false,
    },
    Server {
        commands: &[&["bash-language-server", "start"]],
        languages: &[("sh", "shellscript")],
        needs_folder: false,
    },
    Server {
        commands: &[&["zls"]],
        languages: &[("zig", "zig")],
        needs_folder: false,
    },
    Server {
        commands: &[&["lua-language-server"]],
        languages: &[("lua", "lua")],
        needs_folder: false,
    },
    Server {
        commands: &[&["taplo", "lsp", "stdio"]],
        languages: &[("toml", "toml")],
        needs_folder: true,
    },
    Server {
        commands: &[&["vscode-json-language-server", "--stdio"]],
        languages: &[("json", "json")],
        needs_folder: false,
    },
    Server {
        commands: &[&["yaml-language-server", "--stdio"]],
        languages: &[("yaml", "yaml")],
        needs_folder: false,
    },
    Server {
        commands: &[&["vscode-html-language-server", "--stdio"]],
        languages: &[("html", "html")],
        needs_folder: false,
    },
    Server {
        commands: &[&["vscode-css-language-server", "--stdio"]],
        languages: &[("css", "css")],
        needs_folder: false,
    },
];

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
    report: Report,
    progress: Rc<Progress>,
}

impl Servers {
    /// Servers that tell `report` of the problems they meet.
    pub fn new(report: impl Fn(&str) + 'static) -> Self {
        Self {
            root: RefCell::default(),
            clients: RefCell::default(),
            environment: RefCell::default(),
            report: Rc::new(report),
            progress: Progress::new(),
        }
    }

    /// Shows what the servers are busy with.
    pub fn indicator(&self) -> &gtk::Revealer {
        self.progress.widget()
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
        if server.needs_folder && root.is_none() {
            return;
        }
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
            if environment
                .peek()
                .is_some_and(|environment| environment.find(server.commands).is_none())
            {
                return;
            }
            let client = Client::start(
                server,
                root.as_ref(),
                environment,
                Rc::clone(&self.report),
                Rc::clone(&self.progress),
            );
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
        let (root, report) = (self.root.borrow().clone(), Rc::clone(&self.report));
        let environment = async move {
            let Some(root) = root else {
                return Rc::new(Environment::default());
            };
            Rc::new(Environment::load(&root).await.unwrap_or_else(|message| {
                report(&message);
                Environment::failed()
            }))
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
