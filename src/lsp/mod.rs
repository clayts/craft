//! Language server support: a window runs a server per language and project,
//! found on the project's `PATH`, and connects each open file to its server.

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

/// A window's language servers, by server and project root.
pub struct Servers {
    clients: RefCell<HashMap<(usize, glib::GString), Rc<Client>>>,
    environments: RefCell<HashMap<glib::GString, SharedEnvironment>>,
    report: Report,
    progress: Rc<Progress>,
}

impl Servers {
    /// Servers that tell `report` of the problems they meet.
    pub fn new(report: impl Fn(&str) + 'static) -> Self {
        Self {
            clients: RefCell::default(),
            environments: RefCell::default(),
            report: Rc::new(report),
            progress: Progress::new(),
        }
    }

    /// Shows what the servers are busy with.
    pub fn indicator(&self) -> &gtk::Revealer {
        self.progress.widget()
    }

    /// Connects `editor` to the server for its language, starting it for the
    /// project at `root` if needed.
    pub fn attach(&self, editor: &Editor, root: &gio::File) {
        let Some((index, language)) = editor.language().and_then(|name| server_for(&name)) else {
            return;
        };
        let key = (index, root.uri());
        let running = self
            .clients
            .borrow()
            .get(&key)
            .filter(|client| !client.is_stopped())
            .cloned();
        let client = if let Some(client) = running {
            client
        } else {
            let server = &SERVERS[index];
            let environment = self.environment(root);
            // Once the environment is known, a missing server is too.
            if environment
                .peek()
                .is_some_and(|environment| environment.find(server.commands).is_none())
            {
                return;
            }
            let client = Client::start(
                server,
                root,
                environment,
                Rc::clone(&self.report),
                Rc::clone(&self.progress),
            );
            let stopped = self.clients.borrow_mut().insert(key, Rc::clone(&client));
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

    /// The environment of the project at `root`, loaded once, or again after
    /// failing, in case the cause has been fixed.
    fn environment(&self, root: &gio::File) -> SharedEnvironment {
        let key = root.uri();
        if let Some(environment) = self.environments.borrow().get(&key)
            && !environment
                .peek()
                .is_some_and(|environment| environment.failed)
        {
            return environment.clone();
        }
        let (root, report) = (root.clone(), Rc::clone(&self.report));
        let environment = async move {
            Rc::new(Environment::load(&root).await.unwrap_or_else(|message| {
                report(&message);
                Environment::failed()
            }))
        }
        .boxed_local()
        .shared();
        self.environments
            .borrow_mut()
            .insert(key, environment.clone());
        environment
    }

    /// Shuts every server down, keeping the application running until they
    /// have exited.
    pub fn shutdown(&self) {
        self.environments.take();
        let application = gio::Application::default();
        for client in self.clients.take().into_values() {
            let hold = application
                .as_ref()
                .map(gio::prelude::ApplicationExtManual::hold);
            glib::spawn_future_local(async move {
                client.shutdown().await;
                drop(hold);
            });
        }
    }
}
