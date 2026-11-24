//! The phone's launcher (packaging/ios/Launcher), answered: it speaks the control socket's language through
//! `horizon_ios_launcher`'s queue, so `worlds`, `open`, `connect`, `options-set` and the rest mean what they
//! mean to horizon-ctl. Four lines are its own: `launcher-start`, which lets the game load (it waits for the
//! player to start it); `launcher-state`, which says whether the game waits to be started, is loading, waits
//! for files or is at its title; `import PATH`, which installs an archive the launcher downloaded; and
//! `launcher-world {json}`, Select World's actions for folders with spaces in their names.
//!
//! The loading window answers the start, the state and imports before there is a `Game` to ask; anything else
//! waits for the title.
use super::event_loop::EventLoopCtl;
use super::{Game, Loading};
use horizon_client::{
    Result,
    import::{Installed, Kind, Pack},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
};

/// The portrait launcher and landscape game share a UIWindow. Permit the
/// launcher's orientation before acknowledging its presentation request.
fn launcher_visibility(window: Option<&winit::window::Window>, visible: bool) -> bool {
    if !visible {
        if horizon_ios_launcher::cancellation_requested() {
            return false;
        }
        horizon_ios_launcher::commit_start();
    }
    horizon_ios_launcher::set_covered(visible);
    #[cfg(target_os = "ios")]
    if let Some(window) = window {
        use winit::platform::ios::{ValidOrientations, WindowExtIOS};
        window.set_preferred_frame_rate(if visible { None } else { Some(120) });
        window.set_valid_orientations(if visible {
            ValidOrientations::LandscapeAndPortrait
        } else {
            ValidOrientations::Landscape
        });
    }
    #[cfg(not(target_os = "ios"))]
    let _ = window;
    true
}

enum ImportCompletion {
    Installed(Installed),
    AlreadyInstalled(PathBuf),
}

/// What came back for a launcher request so far: a control answer, or an import's result.
enum Waiting {
    Control(Receiver<String>),
    WorldFile {
        answers: Receiver<Value>,
        edits_list: bool,
    },
    Import(Receiver<std::result::Result<ImportCompletion, String>>),
}

thread_local! {
    /// Launcher requests whose answers have not come back yet. Only the main thread touches them.
    static PENDING: RefCell<Vec<(Waiting, horizon_ios_launcher::Request)>> = const { RefCell::new(Vec::new()) };
}

#[derive(Default)]
struct Delivery {
    resources: Option<PathBuf>,
    worlds_changed: bool,
}

/// Answer completed requests and report changes to the title or loader.
fn deliver() -> Delivery {
    let mut delivered = Delivery::default();
    let pending = PENDING.with_borrow_mut(std::mem::take);
    let mut still = Vec::new();
    for (waiting, request) in pending {
        match waiting {
            Waiting::Control(answers) => match answers.try_recv() {
                Ok(answer) => request.answer(&answer),
                // Dropping the request answers it with an error.
                Err(TryRecvError::Disconnected) => {}
                Err(TryRecvError::Empty) => still.push((Waiting::Control(answers), request)),
            },
            Waiting::WorldFile {
                answers,
                edits_list,
            } => match answers.try_recv() {
                Ok(answer) => {
                    delivered.worlds_changed |= edits_list;
                    request.answer(&answer.to_string());
                }
                Err(TryRecvError::Disconnected) => {}
                Err(TryRecvError::Empty) => still.push((
                    Waiting::WorldFile {
                        answers,
                        edits_list,
                    },
                    request,
                )),
            },
            Waiting::Import(result) => match result.try_recv() {
                Ok(Ok(ImportCompletion::Installed(installed))) => {
                    request.answer(&installed_json(&installed).to_string());
                    match installed.kind {
                        Kind::Resources => delivered.resources = Some(installed.path),
                        Kind::World => delivered.worlds_changed = true,
                        Kind::ResourcePack => {}
                    }
                }
                Ok(Ok(ImportCompletion::AlreadyInstalled(root))) => {
                    // The running loader needs the path even when this request did not install it.
                    delivered.resources = Some(root);
                    request.answer(r#"{"ok":true,"kind":"resources_already_installed"}"#);
                }
                Ok(Err(error)) => request.answer(&json!({ "error": error }).to_string()),
                Err(TryRecvError::Disconnected) => {}
                Err(TryRecvError::Empty) => still.push((Waiting::Import(result), request)),
            },
        }
    }
    PENDING.with_borrow_mut(|pending| pending.extend(still));
    delivered
}

fn start_owner(line: &str) -> Option<&str> {
    line.split_whitespace().nth(1)
}

/// The configured prepared-file source is independent of the engine phase.
fn game_files_source() -> Value {
    let version = horizon_core::VANILLA_VERSION;
    json!({
        "url": horizon_client::import::download::url(version),
        "version": version,
    })
}

/// Export needs the active log directory and a best-effort writer barrier.
fn reply_diagnostic_log_home(request: horizon_ios_launcher::Request) {
    // The reply callback is thread-safe. Keep filesystem waits off the event
    // loop, including when only status operations may run during cleanup.
    let _ = std::thread::Builder::new()
        .name("diagnostics-flush".into())
        .spawn(move || {
            let flushed = horizon_client::logging::flush_for_export();
            let home = horizon_client::logging::game_home();
            let home = horizon_client::logging::player_dir_override().unwrap_or(&home);
            request.answer(&json!({ "path": home.to_str(), "flushed": flushed }).to_string());
        });
}

fn allowed_while_stopping(command: &str) -> bool {
    matches!(
        command,
        "launcher-state"
            | "launcher-visible"
            | "launcher-cancel-start"
            | "launcher-session"
            | "download-state"
            | "game-files-source"
            | "diagnostic-log-home"
            | "options-get"
            | "worlds"
            | "servers"
            | "server-ping"
    )
}

fn import_allowed_while_loading(launcher_idle: bool, recovering_files: bool) -> bool {
    launcher_idle || recovering_files
}

fn commit_start(request: horizon_ios_launcher::Request) {
    let committed =
        start_owner(&request.line).is_some_and(horizon_ios_launcher::commit_start_owned);
    request.answer(
        &json!({ "ok": committed, "status": if committed { "committed" } else { "stale" } })
            .to_string(),
    );
}

fn cancel_start(request: horizon_ios_launcher::Request) {
    let Some(owner) = start_owner(&request.line).map(str::to_owned) else {
        request.answer(r#"{"error":"a launch identity is required"}"#);
        return;
    };
    // A visible game has already passed the native presentation commit point.
    if !horizon_ios_launcher::covered() {
        horizon_ios_launcher::commit_start();
    }
    match horizon_ios_launcher::request_cancel(&owner) {
        horizon_ios_launcher::CancelDecision::Accepted => {
            horizon_ios_launcher::wait_for_cancel_reply(owner, request);
        }
        horizon_ios_launcher::CancelDecision::Stale => {
            request.answer(r#"{"ok":true,"status":"stale"}"#);
        }
        horizon_ios_launcher::CancelDecision::Committed => {
            request.answer(r#"{"ok":true,"status":"committed"}"#);
        }
    }
}

/// A native import owns its destination until its completion is delivered.
fn importing_archive() -> bool {
    PENDING.with_borrow(|pending| {
        pending
            .iter()
            .any(|(waiting, _)| matches!(waiting, Waiting::Import(_)))
    })
}

/// Install the archive named by `import PATH` on a worker: a prepared tree is hundreds of megabytes.
fn start_import(request: horizon_ios_launcher::Request) {
    if importing_archive() {
        request.answer(r#"{"error":"Wait for the current import to finish."}"#);
        return;
    }
    let command = request.line.split_whitespace().next().unwrap_or_default();
    let resources_only = command == "import-game-files";
    let argument = request.line.trim_start()[command.len()..].trim();
    let (path, expected_marker) = if resources_only && argument.starts_with('{') {
        let fields = serde_json::from_str::<Value>(argument).ok();
        let parsed = fields.as_ref().and_then(|fields| {
            Some((
                fields["path"].as_str()?,
                fields["resource_marker"].as_str()?,
            ))
        });
        let Some((path, marker)) = parsed else {
            request.answer(r#"{"error":"Invalid automatic game-file install request."}"#);
            return;
        };
        (PathBuf::from(path), Some(marker.to_owned()))
    } else {
        (PathBuf::from(argument), None)
    };
    if path.as_os_str().is_empty() {
        request.answer(r#"{"error":"import PATH: the archive to install"}"#);
        return;
    }
    let home = horizon_client::import::home();
    let (sender, receiver) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("horizon-import".into())
        .spawn(move || {
            let installed = match expected_marker {
                Some(marker) => {
                    horizon_client::import::install_game_files_if_current(&path, &home, &marker)
                }
                None => install_archive(&path, &home, resources_only).map(Some),
            };
            let completed = installed.and_then(|installed| match installed {
                Some(installed) => Ok(ImportCompletion::Installed(installed)),
                None => {
                    let root = home.join(Kind::Resources.folder());
                    if let Some(problem) = horizon_client::import::damaged_prepared(&root) {
                        return Err(
                            format!("The existing game files are damaged: {problem}").into()
                        );
                    }
                    Ok(ImportCompletion::AlreadyInstalled(root))
                }
            });
            let _ = sender.send(completed.map_err(|error| error.to_string()));
        });
    match spawned {
        Ok(_) => PENDING.with_borrow_mut(|p| p.push((Waiting::Import(receiver), request))),
        Err(error) => request.answer(&json!({ "error": error.to_string() }).to_string()),
    }
}

fn install_archive(
    path: &std::path::Path,
    home: &std::path::Path,
    resources_only: bool,
) -> Result<Installed> {
    let pack = Pack::open(path)?;
    let pack = if resources_only {
        pack.require_game_files()?
    } else {
        pack
    };
    pack.install(home)
}

fn installed_json(installed: &Installed) -> Value {
    json!({
        "ok": true,
        "kind": match installed.kind {
            Kind::Resources => "resources",
            Kind::World => "world",
            Kind::ResourcePack => "resource_pack",
        },
        "name": installed.path.file_name().map(|n| n.to_string_lossy().into_owned()),
        "files": installed.files,
        "bytes": installed.bytes,
    })
}

impl Game<'_> {
    /// The launcher's requests, once a frame: its own lines here, the rest through the control socket's
    /// dispatcher, as if horizon-ctl had sent them.
    pub(super) fn poll_launcher(&mut self, event_loop: &dyn EventLoopCtl) {
        for request in horizon_ios_launcher::take() {
            let command = request.line.split_whitespace().next().unwrap_or_default();
            if self.quit_to_launcher && !allowed_while_stopping(command) {
                request.answer(r#"{"error":"The game is stopping. Try again shortly."}"#);
                continue;
            }
            match command {
                "launcher-visible" => {
                    let accepted = launcher_visibility(
                        self.window.as_deref(),
                        request.line.ends_with(" true"),
                    );
                    request.answer(&json!({ "ok": accepted }).to_string());
                }

                "launcher-state" => {
                    let state = json!({
                        "phase": if self.title.is_some() { "title" } else { "world" },
                        "screen": self.screen_name(),
                        "player": self.options.player_name(),
                        "version": horizon_core::version::VERSION_ID,
                    });
                    request.answer(&state.to_string());
                }
                "game-files-source" => request.answer(&game_files_source().to_string()),
                "diagnostic-log-home" => reply_diagnostic_log_home(request),
                "import" | "import-game-files" => start_import(request),
                // The game is up: the download screen has nothing left to show.
                "download-state" => request.answer(r#"{"phase":"done"}"#),
                "launcher-retry-loader" => request.answer(r#"{"ok":true}"#),
                "launcher-start" => {
                    let accepted = !self.quit_to_launcher
                        && horizon_ios_launcher::start_owned(start_owner(&request.line));
                    request.answer(
                        &json!({
                            "ok": accepted,
                            "phase": if self.title.is_some() { "title" } else { "world" },
                        })
                        .to_string(),
                    );
                }
                "launcher-commit-start" => commit_start(request),
                "launcher-cancel-start" => {
                    // World entry is a committed action. A stale native task
                    // must not tear down an already opened world or server.
                    if self.title.is_none() {
                        horizon_ios_launcher::commit_start();
                    }
                    cancel_start(request);
                    if horizon_ios_launcher::cancellation_requested() {
                        self.quit_to_launcher = true;
                    }
                }
                "launcher-session" => session_request(request),
                "launcher-world" => self.launcher_world(request),
                _ => {
                    let (reply, answers) = mpsc::channel();
                    let line = request.line.clone();
                    self.answer(horizon_client::control::Request { line, reply }, event_loop);
                    PENDING.with_borrow_mut(|p| p.push((Waiting::Control(answers), request)));
                }
            }
        }
        // Game files that arrive while the game runs are read on its next start.
        if deliver().worlds_changed
            && let Some(title) = self.title.as_mut()
        {
            title.worlds_changed();
        }
    }

    /// `launcher-world`: Select World's Play by folder here; the rest is files (`world_files`).
    fn launcher_world(&mut self, request: horizon_ios_launcher::Request) {
        let Some(request) = world_files(request) else {
            return;
        };
        // Only Play is left: it needs the title screen.
        let args = world_args(&request.line);
        let folder = args["folder"].as_str().unwrap_or_default().to_owned();
        let answer = if self.title.is_none() {
            json!({ "error": "a world is already open" })
        } else {
            match self.open_world(&folder, None, None, false, None) {
                Ok(()) => json!({ "ok": true }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        };
        request.answer(&answer.to_string());
    }
}

/// `launcher-world {"action": "open"|"rename"|"backup"|"delete", "folder": ..., "name": ...}`'s arguments: Select
/// World and Edit World by folder, for folders the control socket's word-split lines cannot name ("New World").
fn world_args(line: &str) -> Value {
    line.trim_start()
        .strip_prefix("launcher-world")
        .and_then(|rest| serde_json::from_str(rest).ok())
        .unwrap_or_default()
}

/// Rename, Back Up and Delete, which are files and need no game; the request back for Play, which does.
fn world_files(request: horizon_ios_launcher::Request) -> Option<horizon_ios_launcher::Request> {
    let args = world_args(&request.line);
    let (Some(action), Some(folder)) = (args["action"].as_str(), args["folder"].as_str()) else {
        request.answer(r#"{"error":"launcher-world {\"action\", \"folder\"}"}"#);
        return None;
    };
    let saves = std::env::current_dir().unwrap_or_default().join("saves");
    let folder = folder.to_owned();
    match action {
        "open" => return Some(request),
        "rename" => {
            let name = args["name"].as_str().unwrap_or_default().trim().to_owned();
            if name.is_empty() {
                request.answer(r#"{"error":"a world needs a name"}"#);
            } else {
                start_world_file_operation(request, true, move || {
                    world_edit_reply(horizon_client::title::world_management::rename(
                        &saves, &folder, &name,
                    ))
                });
            }
        }
        "delete" => start_world_file_operation(request, true, move || {
            world_edit_reply(horizon_client::title::world_management::delete(
                &saves, &folder,
            ))
        }),
        "backup" => start_world_file_operation(request, false, move || {
            backup_reply(
                &folder,
                horizon_client::title::world_management::backup(&saves, &folder),
            )
        }),
        _ => request
            .answer(&json!({ "error": format!("launcher-world: no action {action}") }).to_string()),
    }
    None
}

fn world_edit_reply(result: Result<()>) -> Value {
    match result {
        Ok(()) => json!({ "ok": true }),
        Err(error) => json!({ "error": error.to_string() }),
    }
}

/// Saving metadata or retrying filesystem errors can stall the launcher just
/// as a backup can. The world operation owns its session lock on this worker.
fn start_world_file_operation(
    request: horizon_ios_launcher::Request,
    edits_list: bool,
    operation: impl FnOnce() -> Value + Send + 'static,
) {
    let (reply, answers) = mpsc::channel();
    match std::thread::Builder::new()
        .name("horizon-world-files".into())
        .spawn(move || {
            let _ = reply.send(operation());
        }) {
        Ok(_) => PENDING.with_borrow_mut(|p| {
            p.push((
                Waiting::WorldFile {
                    answers,
                    edits_list,
                },
                request,
            ))
        }),
        Err(error) => request.answer(&json!({ "error": error.to_string() }).to_string()),
    }
}

/// `launcher-session {"name", "uuid", "access_token", "user_type": "msa", "xuid"}`: the Microsoft account the
/// phone's launcher signed in (the session file's fields, docs/microsoft-account.md), kept in memory only;
/// `launcher-session off` plays offline again. The token is never echoed back.
fn session_request(request: horizon_ios_launcher::Request) {
    let rest = request
        .line
        .trim_start()
        .strip_prefix("launcher-session")
        .map(str::trim)
        .unwrap_or_default();
    let answer = if rest == "off" {
        horizon_client::auth::clear_current();
        json!({ "ok": true })
    } else {
        match horizon_client::auth::AuthSession::parse(rest) {
            Ok(session) => {
                let name = session.name.clone();
                horizon_client::auth::set_current(session);
                json!({ "ok": true, "name": name })
            }
            Err(e) => json!({ "error": e }),
        }
    };
    request.answer(&answer.to_string());
}

/// The control lines that are only files (worlds, servers, pings: `saves_control::file_command`), answered
/// without a game; the request back for anything else.
fn file_request(request: horizon_ios_launcher::Request) -> Option<horizon_ios_launcher::Request> {
    let words: Vec<&str> = request.line.split_whitespace().collect();
    let (reply, answers) = mpsc::channel();
    match super::saves_control::file_command(&words, &request.line, &reply) {
        Ok(None) => Some(request),
        Ok(Some(Some(value))) => {
            request.answer(&value.to_string());
            None
        }
        // Answered later, from a worker.
        Ok(Some(None)) => {
            PENDING.with_borrow_mut(|p| p.push((Waiting::Control(answers), request)));
            None
        }
        Err(e) => {
            request.answer(&json!({ "error": e.to_string() }).to_string());
            None
        }
    }
}

impl Loading {
    /// `options-get KEY...` and `options-set KEY VALUE` before the game exists, on the settings it will be
    /// made with, saved to options.txt as the options screen saves them (game/options.rs does the same in a
    /// running game, where a change also applies at once).
    fn launcher_options(&mut self, line: &str) -> Value {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() == Some(&"options-get") {
            return match self.settings.to_file().describe_options(&words[1..]) {
                Ok(options) => json!({ "options": options }),
                Err(error) => json!({ "error": error }),
            };
        }
        let (key, value) = match horizon_client::options_file::parse_set_command(line) {
            Ok(assignment) => assignment,
            Err(error) => return json!({ "error": error }),
        };
        let settings = match self.settings.with_option(key, value) {
            Ok(settings) => settings,
            Err(error) => return json!({ "error": error }),
        };
        if let Err(error) = settings.save_options(&Game::options_path()) {
            return json!({ "error": format!("Could not save options.txt: {error}") });
        }
        self.settings = settings;
        json!({ "ok": true, "key": key, "option": self.settings.to_file().describe(key) })
    }

    /// The launcher's requests before the game exists: the start, its state, imports (how a phone with no game
    /// files gets them from the launcher), the options, and what is only files: worlds, servers, pings.
    /// Game files that arrive end the wait for them.
    pub(super) fn poll_launcher(&mut self) -> Result<()> {
        // Imports and starts share the game home with the previous engine's
        // finite cache collector. Do not enqueue a new home owner ahead of it.
        let home_ready = horizon_client::mod_content::poll_root_collectors();
        let requests = if home_ready {
            horizon_ios_launcher::take()
        } else {
            horizon_ios_launcher::take_status_requests()
        };
        for request in requests {
            let command = request.line.split_whitespace().next().unwrap_or_default();
            if horizon_ios_launcher::cancellation_requested() && !allowed_while_stopping(command) {
                request.answer(r#"{"error":"The game is stopping. Try again shortly."}"#);
                continue;
            }
            match command {
                "launcher-visible" => {
                    let accepted = launcher_visibility(
                        self.window.as_deref(),
                        request.line.ends_with(" true"),
                    );
                    request.answer(&json!({ "ok": accepted }).to_string());
                }

                "game-files-source" => request.answer(&game_files_source().to_string()),
                "diagnostic-log-home" => reply_diagnostic_log_home(request),
                "launcher-start" => {
                    if importing_archive() {
                        request.answer(r#"{"error":"Wait for the current import to finish."}"#);
                    } else {
                        if horizon_ios_launcher::waiting() {
                            self.refresh_import_root(&horizon_client::import::home())?;
                        }
                        let accepted =
                            horizon_ios_launcher::start_owned(start_owner(&request.line));
                        request.answer(&json!({ "ok": accepted, "phase": "loading" }).to_string());
                    }
                }
                "launcher-commit-start" => commit_start(request),
                "launcher-cancel-start" => cancel_start(request),
                "launcher-state" => {
                    let mut state = if horizon_ios_launcher::waiting() {
                        let home = horizon_client::import::home();
                        let root =
                            horizon_client::import::resources_root(&self.vanilla_root, Some(&home));
                        let problem = if home_ready {
                            root.as_deref()
                                .and_then(horizon_client::import::damaged_prepared)
                        } else {
                            None
                        };
                        json!({
                            "phase": "waiting",
                            "game_files_present": root.is_some(),
                            "game_files_problem": problem,
                            "stopped_owner": horizon_ios_launcher::stopped_owner(),
                        })
                    } else if self.importing() {
                        json!({
                            "phase": "needs_files",
                            "reason": self.import_reason(),
                            "retry_loader": self.can_retry_loader(),
                        })
                    } else {
                        json!({ "phase": "loading", "progress": self.progress.fraction() })
                    };
                    if !home_ready {
                        state["cache_cleanup_in_progress"] = json!(true);
                    }
                    state["version"] = json!(horizon_core::version::VERSION_ID);
                    request.answer(&state.to_string());
                }
                "download-state" => request.answer(&self.download_state().to_string()),
                "launcher-retry-loader" | "download-retry" if importing_archive() => {
                    // Keep recovery active so delivery can restart on the imported root.
                    request.answer(r#"{"error":"Wait for the current import to finish."}"#);
                }
                "launcher-retry-loader" => {
                    // A retry reports failure while keeping recovery open.
                    let result = if self.can_retry_loader() {
                        self.retry_installed_files()
                    } else {
                        Ok(())
                    };
                    let answer = match result {
                        Ok(()) => json!({ "ok": true }),
                        Err(error) => json!({ "error": error.to_string() }),
                    };
                    request.answer(&answer.to_string());
                }
                "download-retry" => {
                    let answer = match self.retry_download() {
                        Ok(()) => json!({ "ok": true }),
                        Err(e) => json!({ "error": e.to_string() }),
                    };
                    request.answer(&answer.to_string());
                }
                "import" | "import-game-files" => {
                    if import_allowed_while_loading(
                        horizon_ios_launcher::waiting(),
                        self.importing(),
                    ) {
                        start_import(request);
                    } else {
                        request.answer(r#"{"error":"The game is starting. Wait for startup to finish before importing files."}"#);
                    }
                }
                "launcher-session" => session_request(request),
                "options-get" | "options-set" => {
                    let answer = self.launcher_options(&request.line);
                    request.answer(&answer.to_string());
                }
                "shader-pack" | "shader-pack-set" | "shader-pack-status" => {
                    let words: Vec<&str> = request.line.split_whitespace().collect();
                    let answer = match super::shader_packs::command(
                        &mut self.settings,
                        &words,
                        &request.line,
                    ) {
                        Ok(value) => value.map(|(response, _)| response).unwrap_or_default(),
                        Err(e) => json!({ "error": e.to_string() }),
                    };
                    request.answer(&answer.to_string());
                }
                "launcher-world" => {
                    if let Some(request) = world_files(request) {
                        request.answer(
                            &json!({ "error": "start the game to play a world", "starting": true })
                                .to_string(),
                        );
                    }
                }
                _ => {
                    if let Some(request) = file_request(request) {
                        request.answer(
                            &json!({ "error": "the game is still starting", "starting": true })
                                .to_string(),
                        );
                    }
                }
            }
        }
        if home_ready {
            let delivered = deliver();
            if delivered.worlds_changed {
                self.menu.worlds_changed();
            }
            if let Some(root) = delivered.resources
                && self.importing()
                && !horizon_ios_launcher::cancellation_requested()
            {
                self.files_arrived(root)?;
            }
        }
        Ok(())
    }
}

/// Keep the launcher's byte-count field numeric as the engine also returns the archive path.
fn backup_reply(folder: &str, result: Result<(std::path::PathBuf, u64)>) -> Value {
    match result {
        Ok((path, bytes)) => json!({ "ok": true, "path": path, "bytes": bytes }),
        Err(error) => json!({ "error": format!("backup of {folder} failed: {error}") }),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn imports_are_available_during_idle_or_recovery_but_not_during_transitions() {
        for command in ["import", "import-game-files"] {
            assert!(!super::allowed_while_stopping(command));
        }
        assert!(super::allowed_while_stopping("launcher-state"));
        assert!(super::allowed_while_stopping("launcher-cancel-start"));
        assert!(super::import_allowed_while_loading(true, false));
        assert!(super::import_allowed_while_loading(false, true));
        assert!(!super::import_allowed_while_loading(false, false));
    }

    #[test]
    fn game_files_source_describes_the_expected_archive_version() {
        let answer = super::game_files_source();
        let version = horizon_core::VANILLA_VERSION;
        assert_eq!(answer["version"], version);
        assert_eq!(
            answer["url"],
            serde_json::json!(horizon_client::import::download::url(version))
        );
    }

    #[test]
    fn already_installed_game_files_notify_the_waiting_loader() {
        let (sender, receiver) = super::mpsc::channel();
        let (reply, answer) = super::mpsc::channel();
        let root = super::PathBuf::from("fixture/resources");
        super::PENDING.with_borrow_mut(|pending| {
            pending.push((
                super::Waiting::Import(receiver),
                horizon_ios_launcher::Request::new("fixture", move |json| {
                    reply.send(json.to_owned()).unwrap();
                }),
            ));
        });
        assert!(super::deliver().resources.is_none());
        sender
            .send(Ok(super::ImportCompletion::AlreadyInstalled(root.clone())))
            .unwrap();
        assert_eq!(super::deliver().resources, Some(root));
        assert_eq!(
            answer.try_recv().unwrap(),
            r#"{"ok":true,"kind":"resources_already_installed"}"#
        );
    }

    #[test]
    fn launcher_world_file_operation_replies_after_the_worker_finishes() {
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };

        let (started, worker_started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (reply, answered) = mpsc::channel();
        let caller = std::thread::current().id();
        super::start_world_file_operation(
            horizon_ios_launcher::Request::new("fixture", move |json| {
                reply.send(json.to_owned()).unwrap();
            }),
            true,
            move || {
                started.send(std::thread::current().id()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                serde_json::json!({ "ok": true })
            },
        );
        assert_ne!(
            worker_started.recv_timeout(Duration::from_secs(5)).unwrap(),
            caller
        );
        assert!(!super::deliver().worlds_changed);
        assert!(matches!(
            answered.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let delivered = super::deliver();
            if let Ok(reply) = answered.try_recv() {
                assert!(delivered.worlds_changed);
                assert_eq!(reply, r#"{"ok":true}"#);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "world operation did not acknowledge completion"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn backup_reply_keeps_size_numeric_and_does_not_report_failed_backups_as_success() {
        let reply = super::backup_reply(
            "World",
            Ok((std::path::PathBuf::from("backups/World.zip"), 42)),
        );
        assert_eq!(reply["bytes"].as_u64(), Some(42));
        assert!(reply["path"].as_str().unwrap().ends_with("World.zip"));
        let failed = super::backup_reply(
            "World",
            Err(std::io::Error::other("fixture backup failure").into()),
        );
        assert!(
            failed["error"]
                .as_str()
                .unwrap()
                .contains("fixture backup failure")
        );
        assert!(failed.get("bytes").is_none());
    }

    #[test]
    fn automatic_import_rejects_world_before_writing_but_manual_import_allows_it() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "horizon-auto-import-{}-{nonce}",
            std::process::id()
        ));
        let world = root.join("world");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("level.dat"), b"synthetic world fixture").unwrap();
        let archive = root.join("world.zip");
        horizon_client::import::pack(&world, &archive, |_, _| {}).unwrap();
        let home = root.join("home");
        let error = super::install_archive(&archive, &home, true).unwrap_err();
        assert!(
            error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidData)
        );
        assert!(
            !home.exists(),
            "Wrong-kind download must not touch player data"
        );
        assert!(archive.exists());
        let installed = super::install_archive(&archive, &home, false).unwrap();
        assert_eq!(installed.kind, horizon_client::import::Kind::World);
        assert!(installed.path.join("level.dat").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}
