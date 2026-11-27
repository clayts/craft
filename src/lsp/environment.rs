//! The environment language servers run in: Craft's own, changed as direnv
//! changes a shell's on entering the project folder, so that servers come
//! from the project's development shell where it has one.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use gtk::{gio, glib};
use serde::Deserialize;

use super::status::{Level, Status};

/// The program, which is also the tool's name.
const DIRENV: &str = "direnv";

/// How direnv regards an `.envrc`: allowed, or blocked until allowed. One
/// that is denied it passes over.
const ALLOWED: u8 = 0;
const BLOCKED: u8 = 1;

#[derive(Debug, Default, PartialEq)]
pub struct Environment {
    /// Variables to set, or to unset where `None`.
    changes: HashMap<String, Option<String>>,
    /// Whether direnv failed to give the project's environment.
    pub failed: bool,
}

impl Environment {
    /// The environment of the project folder `dir`, telling `status` how
    /// loading it went. Where direnv cannot give it, that is Craft's own.
    pub async fn load(dir: &Path, status: &Rc<Status>) -> Self {
        Self::try_load(dir, status).await.unwrap_or_else(|message| {
            status.log(DIRENV, Level::Error, &message);
            Self {
                failed: true,
                ..Self::default()
            }
        })
    }

    /// As [`Self::load`], or why direnv cannot give the environment.
    async fn try_load(dir: &Path, status: &Rc<Status>) -> Result<Self, String> {
        if glib::find_program_in_path(DIRENV).is_none() {
            // Whether there is an `.envrc` that direnv would find.
            let envrc = dir.ancestors().any(|dir| dir.join(".envrc").exists());
            return if envrc {
                Err("Not found on the PATH, so .envrc goes unused".to_owned())
            } else {
                Ok(Self::default())
            };
        }
        let fail = |detail: &dyn Display| format!("Could not load environment\n{detail}");
        let allowed = allowed(dir).await.map_err(|err| fail(&err))?;
        if allowed == Some(BLOCKED) {
            return Err(".envrc blocked; run “direnv allow” to use it".to_owned());
        }
        let busy = (allowed == Some(ALLOWED)).then(|| status.busy(DIRENV, "Loading environment"));
        // Without an `.envrc` too, to undo that of any folder Craft was
        // started in.
        let json = run(dir, ["export", "json"])
            .await
            .map_err(|err| fail(&err))?;
        drop(busy);
        let environment = Self::parse(&json).map_err(|err| fail(&err))?;
        status.started(DIRENV);
        Ok(environment)
    }

    /// Reads the output of `direnv export json`, which is empty when there is
    /// nothing to change.
    fn parse(json: &str) -> serde_json::Result<Self> {
        if json.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(Self {
            changes: serde_json::from_str(json)?,
            failed: false,
        })
    }

    /// The `PATH` that servers are looked for on.
    pub fn path(&self) -> Option<OsString> {
        match self.changes.get("PATH") {
            Some(path) => path.as_ref().map(OsString::from),
            None => std::env::var_os("PATH"),
        }
    }

    /// The first of `commands` whose program is on the environment's `PATH`,
    /// with the program's path.
    pub fn find<'a>(&self, commands: &'a [&'a [&'a str]]) -> Option<(PathBuf, &'a [&'a str])> {
        let path = self.path()?;
        commands.iter().find_map(|&command| {
            let name = command.first()?;
            let program = std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(name))
                .find(|file| is_executable(file))?;
            Some((program, command))
        })
    }

    pub fn apply(&self, command: &mut Command) {
        for (name, value) in &self.changes {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
    }
}

/// How direnv regards the `.envrc` it finds for `dir`, the folder's own or
/// else its nearest parent's, if it finds one.
async fn allowed(dir: &Path) -> Result<Option<u8>, String> {
    let json = run(dir, ["status", "--json"]).await?;
    parse_allowed(&json).map_err(|err| err.to_string())
}

/// Reads how `direnv status --json` regards the `.envrc` it found, if any.
fn parse_allowed(json: &str) -> serde_json::Result<Option<u8>> {
    let status: serde_json::Value = serde_json::from_str(json)?;
    Option::deserialize(&status["state"]["foundRC"]["allowed"])
}

/// Runs `direnv {command} {option}` in `dir`, for what it prints. What it
/// says goes to Craft's standard error, including why it failed, if it did,
/// so the error adds nothing unless direnv could not be run at all.
async fn run(dir: &Path, [command, option]: [&str; 2]) -> Result<glib::GString, String> {
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::STDOUT_PIPE);
    launcher.set_cwd(dir);
    let process = launcher
        .spawn(&[DIRENV, command, option].map(OsStr::new))
        .map_err(|err| err.to_string())?;
    let (stdout, _) = process
        .communicate_utf8_future(None)
        .await
        .map_err(|err| err.to_string())?;
    if process.is_successful() {
        Ok(stdout.unwrap_or_default())
    } else {
        Err(String::new())
    }
}

fn is_executable(file: &Path) -> bool {
    file.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn parses_changes_and_their_absence() {
        assert_eq!(Environment::parse("").unwrap(), Environment::default());
        let environment = Environment::parse(r#"{"A": "1", "B": null}"#).unwrap();
        assert_eq!(environment.changes["A"].as_deref(), Some("1"));
        assert_eq!(environment.changes["B"], None);
        assert!(!environment.failed);
    }

    #[test]
    fn reads_how_direnv_regards_the_envrc() {
        let json = r#"{"config": {}, "state": {
            "foundRC": {"allowed": 2, "path": "/a/.envrc"}, "loadedRC": null}}"#;
        assert_eq!(parse_allowed(json).unwrap(), Some(2));
        let json = r#"{"state": {"foundRC": null, "loadedRC": null}}"#;
        assert_eq!(parse_allowed(json).unwrap(), None);
    }

    #[test]
    fn applies_changes_to_commands() {
        let environment = Environment::parse(r#"{"A": "1", "B": null}"#).unwrap();
        let mut command = Command::new("true");
        environment.apply(&mut command);
        let mut envs: Vec<_> = command.get_envs().collect();
        envs.sort();
        assert_eq!(
            envs,
            [
                (OsStr::new("A"), Some(OsStr::new("1"))),
                (OsStr::new("B"), None)
            ]
        );
    }

    #[test]
    fn finds_executables_on_the_changed_path() {
        let dir = std::env::temp_dir().join(format!("craft-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (server, notes) = (dir.join("server"), dir.join("notes"));
        fs::write(&server, "").unwrap();
        fs::write(&notes, "").unwrap();
        fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();

        let json = serde_json::json!({ "PATH": format!("relative:{}", dir.display()) });
        let environment = Environment::parse(&json.to_string()).unwrap();
        let commands: &[&[&str]] = &[&["missing"], &["notes"], &["server", "--stdio"]];
        assert_eq!(
            environment.find(commands),
            Some((server, &["server", "--stdio"][..]))
        );
        assert_eq!(environment.find(&[&["notes"]]), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
