//! Language server support: a window runs a server per language and project
//! root, found on `PATH`, and connects each open file to its server.

mod client;
mod document;
mod hover;
mod position;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

use client::Client;
pub use document::Document;
pub use position::Encoding;

use crate::editor::Editor;

/// A language server: its command line, and the `GtkSourceView` languages it
/// serves with the LSP identifier of each.
struct Server {
    command: &'static [&'static str],
    languages: &'static [(&'static str, &'static str)],
}

const PYTHON: &[(&str, &str)] = &[("python3", "python"), ("python", "python")];

/// The servers to use, earlier ones first when several serve a language.
const SERVERS: &[Server] = &[
    Server {
        command: &["rust-analyzer"],
        languages: &[("rust", "rust")],
    },
    Server {
        command: &["clangd"],
        languages: &[
            ("c", "c"),
            ("chdr", "c"),
            ("cpp", "cpp"),
            ("cpphdr", "cpp"),
            ("objc", "objective-c"),
        ],
    },
    Server {
        command: &["gopls"],
        languages: &[("go", "go")],
    },
    Server {
        command: &["basedpyright-langserver", "--stdio"],
        languages: PYTHON,
    },
    Server {
        command: &["pyright-langserver", "--stdio"],
        languages: PYTHON,
    },
    Server {
        command: &["pylsp"],
        languages: PYTHON,
    },
    Server {
        command: &["typescript-language-server", "--stdio"],
        languages: &[
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("typescript", "typescript"),
            ("typescript-jsx", "typescriptreact"),
        ],
    },
    Server {
        command: &["nixd"],
        languages: &[("nix", "nix")],
    },
    Server {
        command: &["nil"],
        languages: &[("nix", "nix")],
    },
    Server {
        command: &["bash-language-server", "start"],
        languages: &[("sh", "shellscript")],
    },
    Server {
        command: &["zls"],
        languages: &[("zig", "zig")],
    },
    Server {
        command: &["lua-language-server"],
        languages: &[("lua", "lua")],
    },
    Server {
        command: &["taplo", "lsp", "stdio"],
        languages: &[("toml", "toml")],
    },
    Server {
        command: &["vscode-json-language-server", "--stdio"],
        languages: &[("json", "json")],
    },
    Server {
        command: &["yaml-language-server", "--stdio"],
        languages: &[("yaml", "yaml")],
    },
    Server {
        command: &["vscode-html-language-server", "--stdio"],
        languages: &[("html", "html")],
    },
    Server {
        command: &["vscode-css-language-server", "--stdio"],
        languages: &[("css", "css")],
    },
];

/// The first installed server for the `GtkSourceView` language `language`, by
/// index, with the language's LSP identifier.
fn find_server(language: &str) -> Option<(usize, &'static str)> {
    SERVERS.iter().enumerate().find_map(|(index, server)| {
        let &(_, id) = server
            .languages
            .iter()
            .find(|&&(name, _)| name == language)?;
        glib::find_program_in_path(server.command[0])?;
        Some((index, id))
    })
}

/// A window's language servers, by server and project root.
#[derive(Default)]
pub struct Servers {
    clients: RefCell<HashMap<(usize, glib::GString), Rc<Client>>>,
}

impl Servers {
    /// Connects `editor` to the server for its language, starting it for the
    /// project at `root` if needed. Files in languages without an installed
    /// server stay unconnected.
    pub fn attach(&self, editor: &Editor, root: &gio::File) -> io::Result<()> {
        let Some((index, language)) = editor.language().and_then(|name| find_server(&name)) else {
            return Ok(());
        };
        let key = (index, root.uri());
        let existing = self.clients.borrow().get(&key).cloned();
        let client = match existing {
            Some(client) if !client.is_stopped() => client,
            stopped => {
                let command = SERVERS[index].command;
                let client = Client::start(command, root).map_err(|err| {
                    io::Error::new(err.kind(), format!("Could not start {}: {err}", command[0]))
                })?;
                self.clients.borrow_mut().insert(key, Rc::clone(&client));
                // The new server takes over the stopped one's files.
                for document in stopped.iter().flat_map(|stopped| stopped.documents()) {
                    if let Some(editor) = document.editor() {
                        editor.set_document(Some(Document::new(
                            &client,
                            &editor,
                            document.language(),
                        )));
                    }
                }
                client
            }
        };
        editor.set_document(Some(Document::new(&client, editor, language)));
        Ok(())
    }

    /// Shuts every server down, keeping the application running until they
    /// have exited.
    pub fn shutdown(&self) {
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
