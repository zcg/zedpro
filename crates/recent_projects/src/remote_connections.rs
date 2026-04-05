use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use askpass::EncryptedPassword;
use editor::Editor;
use extension_host::ExtensionStore;
use futures::{FutureExt as _, channel::oneshot, select};
use gpui::{AppContext, AsyncApp, Context, PromptLevel, WindowHandle};

use project::trusted_worktrees;
use remote::{
    DockerConnectionOptions, DockerHost, Interactive, RemoteConnection, RemoteConnectionOptions,
    SshConnectionOptions, WslConnectionOptions,
};
pub use settings::SshConnection;
use settings::{
    DevContainerConnection, DevContainerHost, ExtendingVec, RegisterSetting, RemoteProject,
    Settings, WslConnection,
};
use util::paths::PathWithPosition;
use workspace::notifications::NotificationId;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::{
    AppState, MultiWorkspace, OpenOptions, SerializedWorkspaceLocation, Workspace,
    find_existing_workspace,
};

use crate::disconnected_overlay::is_pending_devcontainer_host_return;

pub use remote_connection::{
    RemoteClientDelegate, RemoteConnectionModal, RemoteConnectionPrompt, SshConnectionHeader,
    connect,
};

#[derive(RegisterSetting)]
pub struct RemoteSettings {
    pub ssh_connections: ExtendingVec<SshConnection>,
    pub wsl_connections: ExtendingVec<WslConnection>,
    pub dev_container_connections: ExtendingVec<DevContainerConnection>,
    /// Whether to read ~/.ssh/config for ssh connection sources.
    pub read_ssh_config: bool,
}

impl RemoteSettings {
    pub fn ssh_connections(&self) -> impl Iterator<Item = SshConnection> + use<> {
        self.ssh_connections.clone().0.into_iter()
    }

    pub fn wsl_connections(&self) -> impl Iterator<Item = WslConnection> + use<> {
        self.wsl_connections.clone().0.into_iter()
    }

    pub fn dev_container_connections(
        &self,
    ) -> impl Iterator<Item = DevContainerConnection> + use<> {
        self.dev_container_connections.clone().0.into_iter()
    }

    pub fn fill_connection_options_from_settings(&self, options: &mut SshConnectionOptions) {
        for conn in self.ssh_connections() {
            if conn.host == options.host.to_string()
                && conn.username == options.username
                && conn.port == options.port
            {
                options.nickname = conn.nickname;
                options.upload_binary_over_ssh = conn.upload_binary_over_ssh.unwrap_or_default();
                options.args = Some(conn.args);
                options.port_forwards = conn.port_forwards;
                break;
            }
        }
    }

    pub fn connection_options_for(
        &self,
        host: String,
        port: Option<u16>,
        username: Option<String>,
    ) -> SshConnectionOptions {
        let mut options = SshConnectionOptions {
            host: host.into(),
            port,
            username,
            ..Default::default()
        };
        self.fill_connection_options_from_settings(&mut options);
        options
    }
}

#[derive(Clone, PartialEq)]
pub enum Connection {
    Ssh(SshConnection),
    Wsl(WslConnection),
    DevContainer(DevContainerConnection),
}

impl From<Connection> for RemoteConnectionOptions {
    fn from(val: Connection) -> Self {
        match val {
            Connection::Ssh(conn) => RemoteConnectionOptions::Ssh(conn.into()),
            Connection::Wsl(conn) => RemoteConnectionOptions::Wsl(conn.into()),
            Connection::DevContainer(conn) => {
                RemoteConnectionOptions::Docker(DockerConnectionOptions {
                    name: conn.name,
                    remote_user: conn.remote_user,
                    container_id: conn.container_id,
                    upload_binary_over_docker_exec: false,
                    use_podman: conn.use_podman,
                    host: docker_host_from_devcontainer_host(conn.host),
                    remote_env: conn.remote_env,
                })
            }
        }
    }
}

fn docker_host_from_devcontainer_host(host: Option<DevContainerHost>) -> DockerHost {
    match host {
        Some(DevContainerHost::Ssh {
            host,
            username,
            port,
            args,
        }) => DockerHost::Ssh(SshConnectionOptions {
            host: host.into(),
            username,
            port,
            args: Some(args),
            ..Default::default()
        }),
        Some(DevContainerHost::Wsl { distro_name, user }) => {
            DockerHost::Wsl(WslConnectionOptions { distro_name, user })
        }
        None => DockerHost::Local,
    }
}

impl From<SshConnection> for Connection {
    fn from(val: SshConnection) -> Self {
        Connection::Ssh(val)
    }
}

impl From<WslConnection> for Connection {
    fn from(val: WslConnection) -> Self {
        Connection::Wsl(val)
    }
}

impl From<DevContainerConnection> for Connection {
    fn from(val: DevContainerConnection) -> Self {
        Connection::DevContainer(val)
    }
}

impl Settings for RemoteSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let remote = &content.remote;
        Self {
            ssh_connections: remote.ssh_connections.clone().unwrap_or_default().into(),
            wsl_connections: remote.wsl_connections.clone().unwrap_or_default().into(),
            dev_container_connections: remote
                .dev_container_connections
                .clone()
                .unwrap_or_default()
                .into(),
            read_ssh_config: remote.read_ssh_config.unwrap(),
        }
    }
}

pub(crate) fn upsert_dev_container_connection(
    connections: &mut Vec<DevContainerConnection>,
    connection: DevContainerConnection,
    starting_dir: String,
    host_starting_dir: Option<String>,
    config_path: Option<String>,
) {
    if let Some(existing) = connections.iter_mut().find(|existing| {
        existing.container_id == connection.container_id
            && existing.use_podman == connection.use_podman
            && existing.host == connection.host
    }) {
        existing.name = connection.name.clone();
        if config_path.is_some() {
            existing.config_path = config_path;
        }
        existing.projects.insert(RemoteProject {
            paths: vec![starting_dir],
        });
        if let Some(host_starting_dir) = host_starting_dir {
            let host_project = RemoteProject {
                paths: vec![host_starting_dir],
            };
            existing.host_projects.insert(host_project.clone());
            existing.last_host_project = Some(host_project);
        }
        return;
    }

    connections.retain(|existing| {
        existing.container_id != connection.container_id
            || existing.use_podman != connection.use_podman
            || existing.host != connection.host
    });

    let mut entry = connection;
    if config_path.is_some() {
        entry.config_path = config_path;
    }
    entry.projects.insert(RemoteProject {
        paths: vec![starting_dir],
    });
    if let Some(host_starting_dir) = host_starting_dir {
        let host_project = RemoteProject {
            paths: vec![host_starting_dir],
        };
        entry.host_projects.insert(host_project.clone());
        entry.last_host_project = Some(host_project);
    }
    connections.insert(0, entry);
}

fn is_missing_dev_container_error(
    connection_options: &RemoteConnectionOptions,
    err_message: &str,
) -> Option<DockerConnectionOptions> {
    let RemoteConnectionOptions::Docker(options) = connection_options else {
        return None;
    };

    if err_message.contains("No such container:") && err_message.contains(&options.container_id) {
        Some(options.clone())
    } else {
        None
    }
}

fn should_suppress_dev_container_error_during_host_return(
    connection_options: &RemoteConnectionOptions,
    cx: &mut AsyncApp,
) -> Option<DockerConnectionOptions> {
    let RemoteConnectionOptions::Docker(options) = connection_options else {
        return None;
    };

    if cx.update(|cx| is_pending_devcontainer_host_return(options, cx)) {
        Some(options.clone())
    } else {
        None
    }
}

fn prune_missing_dev_container_connection(
    app_state: &Arc<AppState>,
    connection_options: &DockerConnectionOptions,
    cx: &mut AsyncApp,
) {
    let fs = app_state.fs.clone();
    let connection_options = connection_options.clone();
    let _ = cx.update(|cx| {
        use gpui::ReadGlobal;
        use settings::SettingsStore;

        SettingsStore::global(cx).update_settings_file(fs, move |setting, _| {
            if let Some(connections) = setting.remote.dev_container_connections.as_mut() {
                connections.retain(|connection| {
                    let options: RemoteConnectionOptions =
                        Connection::DevContainer(connection.clone()).into();
                    !matches!(
                        options,
                        RemoteConnectionOptions::Docker(saved)
                            if saved.container_id == connection_options.container_id
                                && saved.use_podman == connection_options.use_podman
                                && saved.host == connection_options.host
                    )
                });
            }
        });
    });
}

fn show_missing_dev_container_notification(
    workspace: &mut Workspace,
    connection_options: &DockerConnectionOptions,
    err_message: &str,
    cx: &mut Context<Workspace>,
) {
    struct MissingDevContainerError;

    let label = if connection_options.name.is_empty() {
        connection_options.container_id.as_str()
    } else {
        connection_options.name.as_str()
    };
    let message = format!(
        "Saved dev container `{label}` no longer exists and was removed from saved connections.\n{err_message}"
    );
    let notification_id = NotificationId::composite::<MissingDevContainerError>(
        connection_options.container_id.clone(),
    );
    workspace.show_notification(notification_id, cx, |cx| {
        cx.new(|cx| MessageNotification::new(message.clone(), cx))
    });
}

pub async fn open_remote_project(
    connection_options: RemoteConnectionOptions,
    paths: Vec<PathBuf>,
    app_state: Arc<AppState>,
    open_options: workspace::OpenOptions,
    cx: &mut AsyncApp,
) -> Result<()> {
    let created_new_window = open_options.requesting_window.is_none();

    let (existing, open_visible) = find_existing_workspace(
        &paths,
        &open_options,
        &SerializedWorkspaceLocation::Remote(connection_options.clone()),
        cx,
    )
    .await;

    if let Some((existing_window, existing_workspace)) = existing {
        let remote_connection = cx.update(|cx| {
            existing_workspace
                .read(cx)
                .project()
                .read(cx)
                .remote_client()
                .and_then(|client| client.read(cx).remote_connection())
        });

        if let Some(remote_connection) = remote_connection {
            let (resolved_paths, paths_with_positions) =
                determine_paths_with_positions(&remote_connection, paths).await;

            let open_results = existing_window
                .update(cx, |multi_workspace, window, cx| {
                    window.activate_window();
                    multi_workspace.activate(existing_workspace.clone(), window, cx);
                    existing_workspace.update(cx, |workspace, cx| {
                        workspace.open_paths(
                            resolved_paths,
                            OpenOptions {
                                visible: Some(open_visible),
                                ..Default::default()
                            },
                            None,
                            window,
                            cx,
                        )
                    })
                })?
                .await;

            _ = existing_window.update(cx, |multi_workspace, _, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    for item in open_results.iter().flatten() {
                        if let Err(e) = item {
                            workspace.show_error(&e, cx);
                        }
                    }
                });
            });

            let items = open_results
                .into_iter()
                .map(|r| r.and_then(|r| r.ok()))
                .collect::<Vec<_>>();
            navigate_to_positions(&existing_window, items, &paths_with_positions, cx);

            return Ok(());
        }
        // If the remote connection is dead (e.g. server not running after failed reconnect),
        // fall through to establish a fresh connection instead of showing an error.
        log::info!(
            "existing remote workspace found but connection is dead, starting fresh connection"
        );
    }

    let (window, initial_workspace) = if let Some(window) = open_options.requesting_window {
        let workspace = window.update(cx, |multi_workspace, _, _| {
            multi_workspace.workspace().clone()
        })?;
        (window, workspace)
    } else {
        let workspace_position = cx
            .update(|cx| {
                workspace::remote_workspace_position_from_db(connection_options.clone(), &paths, cx)
            })
            .await
            .context("fetching remote workspace position from db")?;

        let mut options =
            cx.update(|cx| (app_state.build_window_options)(workspace_position.display, cx));
        options.window_bounds = workspace_position.window_bounds;

        let window = cx.open_window(options, |window, cx| {
            let project = project::Project::local(
                app_state.client.clone(),
                app_state.node_runtime.clone(),
                app_state.user_store.clone(),
                app_state.languages.clone(),
                app_state.fs.clone(),
                None,
                project::LocalProjectFlags {
                    init_worktree_trust: false,
                    ..Default::default()
                },
                cx,
            );
            let workspace = cx.new(|cx| {
                let mut workspace = Workspace::new(None, project, app_state.clone(), window, cx);
                workspace.centered_layout = workspace_position.centered_layout;
                workspace
            });
            cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
        })?;
        let workspace = window.update(cx, |multi_workspace, _, _cx| {
            multi_workspace.workspace().clone()
        })?;
        (window, workspace)
    };

    loop {
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let delegate = window.update(cx, {
            let paths = paths.clone();
            let connection_options = connection_options.clone();
            let initial_workspace = initial_workspace.clone();
            move |_multi_workspace: &mut MultiWorkspace, window, cx| {
                window.activate_window();
                initial_workspace.update(cx, |workspace, cx| {
                    workspace.hide_modal(window, cx);
                    workspace.toggle_modal(window, cx, |window, cx| {
                        RemoteConnectionModal::new(&connection_options, paths, window, cx)
                    });

                    let ui = workspace
                        .active_modal::<RemoteConnectionModal>(cx)?
                        .read(cx)
                        .prompt
                        .clone();

                    ui.update(cx, |ui, _cx| {
                        ui.set_cancellation_tx(cancel_tx);
                    });

                    Some(Arc::new(RemoteClientDelegate::new(
                        window.window_handle(),
                        ui.downgrade(),
                        if let RemoteConnectionOptions::Ssh(options) = &connection_options {
                            options
                                .password
                                .as_deref()
                                .and_then(|pw| EncryptedPassword::try_from(pw).ok())
                        } else {
                            None
                        },
                    )))
                })
            }
        })?;

        let Some(delegate) = delegate else { break };

        let connection = remote::connect(connection_options.clone(), delegate.clone(), cx);
        let connection = select! {
            _ = cancel_rx => {
                initial_workspace.update(cx, |workspace, cx| {
                    if let Some(ui) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                        ui.update(cx, |modal, cx| modal.finished(cx))
                    }
                });

                break;
            },
            result = connection.fuse() => result,
        };
        let remote_connection = match connection {
            Ok(connection) => connection,
            Err(e) => {
                initial_workspace.update(cx, |workspace, cx| {
                    if let Some(ui) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                        ui.update(cx, |modal, cx| modal.finished(cx))
                    }
                });
                log::error!("Failed to open project: {e:#}");
                let err_message = format!("{e:#}");
                if let Some(connection_options) =
                    should_suppress_dev_container_error_during_host_return(&connection_options, cx)
                {
                    log::info!(
                        "suppressing dev container error while returning to host for {}: {err_message}",
                        connection_options.container_id
                    );
                    if created_new_window {
                        window
                            .update(cx, |_, window, _| window.remove_window())
                            .ok();
                    }
                    return Ok(());
                }
                if let Some(connection_options) =
                    is_missing_dev_container_error(&connection_options, &err_message)
                {
                    prune_missing_dev_container_connection(&app_state, &connection_options, cx);
                    initial_workspace.update(cx, |workspace, cx| {
                        show_missing_dev_container_notification(
                            workspace,
                            &connection_options,
                            &err_message,
                            cx,
                        );
                    });
                    if created_new_window {
                        window
                            .update(cx, |_, window, _| window.remove_window())
                            .ok();
                    }
                    return Ok(());
                }
                let title = match &connection_options {
                    RemoteConnectionOptions::Ssh(_) => "Failed to connect over SSH",
                    RemoteConnectionOptions::Wsl(_) => "Failed to connect to WSL",
                    RemoteConnectionOptions::Docker(_) => "Failed to connect to Dev Container",
                    #[cfg(any(test, feature = "test-support"))]
                    RemoteConnectionOptions::Mock(_) => "Failed to connect to mock server",
                };
                let response = if cfg!(target_os = "windows")
                    && matches!(connection_options, RemoteConnectionOptions::Wsl(_))
                {
                    initial_workspace.update(cx, |workspace, cx| {
                        struct WslConnectError;
                        let notification_id = NotificationId::unique::<WslConnectError>();
                        let message = format!("{title}.\n{err_message}");
                        workspace.show_notification(notification_id, cx, |cx| {
                            cx.new(|cx| MessageNotification::new(message.clone(), cx))
                        });
                    });
                    None
                } else {
                    Some(
                        window
                            .update(cx, |_, window, cx| {
                                window.prompt(
                                    PromptLevel::Critical,
                                    title,
                                    Some(&err_message),
                                    &["Retry", "Cancel"],
                                    cx,
                                )
                            })?
                            .await,
                    )
                };

                if let Some(response) = response {
                    if response == Ok(0) {
                        continue;
                    }
                }

                if created_new_window {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
                return Ok(());
            }
        };

        let (paths, paths_with_positions) =
            determine_paths_with_positions(&remote_connection, paths.clone()).await;

        let opened_items = cx
            .update(|cx| {
                workspace::open_remote_project_with_new_connection(
                    window,
                    remote_connection,
                    cancel_rx,
                    delegate.clone(),
                    app_state.clone(),
                    paths.clone(),
                    cx,
                )
            })
            .await;

        initial_workspace.update(cx, |workspace, cx| {
            if let Some(ui) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                ui.update(cx, |modal, cx| modal.finished(cx))
            }
        });

        match opened_items {
            Err(e) => {
                log::error!("Failed to open project: {e:#}");
                let err_message = format!("{e:#}");
                if let Some(connection_options) =
                    should_suppress_dev_container_error_during_host_return(&connection_options, cx)
                {
                    log::info!(
                        "suppressing dev container error while returning to host for {}: {err_message}",
                        connection_options.container_id
                    );
                    if created_new_window {
                        window
                            .update(cx, |_, window, _| window.remove_window())
                            .ok();
                    }
                    return Ok(());
                }
                if let Some(connection_options) =
                    is_missing_dev_container_error(&connection_options, &err_message)
                {
                    prune_missing_dev_container_connection(&app_state, &connection_options, cx);
                    initial_workspace.update(cx, |workspace, cx| {
                        show_missing_dev_container_notification(
                            workspace,
                            &connection_options,
                            &err_message,
                            cx,
                        );
                    });
                    if created_new_window {
                        window
                            .update(cx, |_, window, _| window.remove_window())
                            .ok();
                    }
                    return Ok(());
                }
                let title = match &connection_options {
                    RemoteConnectionOptions::Ssh(_) => "Failed to connect over SSH",
                    RemoteConnectionOptions::Wsl(_) => "Failed to connect to WSL",
                    RemoteConnectionOptions::Docker(_) => "Failed to connect to Dev Container",
                    #[cfg(any(test, feature = "test-support"))]
                    RemoteConnectionOptions::Mock(_) => "Failed to connect to mock server",
                };
                let response = if cfg!(target_os = "windows")
                    && matches!(connection_options, RemoteConnectionOptions::Wsl(_))
                {
                    initial_workspace.update(cx, |workspace, cx| {
                        struct WslConnectError;
                        let notification_id = NotificationId::unique::<WslConnectError>();
                        let message = format!("{title}.\n{err_message}");
                        workspace.show_notification(notification_id, cx, |cx| {
                            cx.new(|cx| MessageNotification::new(message.clone(), cx))
                        });
                    });
                    None
                } else {
                    Some(
                        window
                            .update(cx, |_, window, cx| {
                                window.prompt(
                                    PromptLevel::Critical,
                                    title,
                                    Some(&err_message),
                                    &["Retry", "Cancel"],
                                    cx,
                                )
                            })?
                            .await,
                    )
                };
                if let Some(response) = response {
                    if response == Ok(0) {
                        continue;
                    }
                }

                if created_new_window {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
                initial_workspace.update(cx, |workspace, cx| {
                    trusted_worktrees::track_worktree_trust(
                        workspace.project().read(cx).worktree_store(),
                        None,
                        None,
                        None,
                        cx,
                    );
                });
            }

            Ok(items) => {
                navigate_to_positions(&window, items, &paths_with_positions, cx);
            }
        }

        break;
    }

    // Register the remote client with extensions. We use `multi_workspace.workspace()` here
    // (not `initial_workspace`) because `open_remote_project_inner` activated the new remote
    // workspace, so the active workspace is now the one with the remote project.
    window
        .update(cx, |multi_workspace: &mut MultiWorkspace, _, cx| {
            let workspace = multi_workspace.workspace().clone();
            workspace.update(cx, |workspace, cx| {
                if let Some(client) = workspace.project().read(cx).remote_client() {
                    if let Some(extension_store) = ExtensionStore::try_global(cx) {
                        extension_store
                            .update(cx, |store, cx| store.register_remote_client(client, cx));
                    }
                }
            });
        })
        .ok();
    Ok(())
}

pub fn navigate_to_positions(
    window: &WindowHandle<MultiWorkspace>,
    items: impl IntoIterator<Item = Option<Box<dyn workspace::item::ItemHandle>>>,
    positions: &[PathWithPosition],
    cx: &mut AsyncApp,
) {
    for (item, path) in items.into_iter().zip(positions) {
        let Some(item) = item else {
            continue;
        };
        let Some(row) = path.row else {
            continue;
        };
        if let Some(active_editor) = item.downcast::<Editor>() {
            window
                .update(cx, |_, window, cx| {
                    active_editor.update(cx, |editor, cx| {
                        let row = row.saturating_sub(1);
                        let col = path.column.unwrap_or(0).saturating_sub(1);
                        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
                            return;
                        };
                        let buffer_snapshot = buffer.read(cx).snapshot();
                        let point = buffer_snapshot.point_from_external_input(row, col);
                        editor.go_to_singleton_buffer_point(point, window, cx);
                    });
                })
                .ok();
        }
    }
}

pub(crate) async fn determine_paths_with_positions(
    remote_connection: &Arc<dyn RemoteConnection>,
    mut paths: Vec<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathWithPosition>) {
    let mut paths_with_positions = Vec::<PathWithPosition>::new();
    for path in &mut paths {
        if let Some(path_str) = path.to_str() {
            let path_with_position = PathWithPosition::parse_str(&path_str);
            if path_with_position.row.is_some() {
                if !path_exists(&remote_connection, &path).await {
                    *path = path_with_position.path.clone();
                    paths_with_positions.push(path_with_position);
                    continue;
                }
            }
        }
        paths_with_positions.push(PathWithPosition::from_path(path.clone()))
    }
    (paths, paths_with_positions)
}

async fn path_exists(connection: &Arc<dyn RemoteConnection>, path: &Path) -> bool {
    let Ok(command) = connection.build_command(
        Some("test".to_string()),
        &["-e".to_owned(), path.to_string_lossy().to_string()],
        &Default::default(),
        None,
        None,
        Interactive::No,
    ) else {
        return false;
    };
    let Ok(mut child) = util::command::new_command(command.program)
        .args(command.args)
        .envs(command.env)
        .spawn()
    else {
        return false;
    };
    child.status().await.is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension::ExtensionHostProxy;
    use fs::FakeFs;
    use gpui::{AppContext, TestAppContext};
    use http_client::BlockedHttpClient;
    use node_runtime::NodeRuntime;
    use remote::RemoteClient;
    use remote_server::{HeadlessAppState, HeadlessProject};
    use serde_json::json;
    use util::path;
    use workspace::find_existing_workspace;

    #[gpui::test]
    async fn test_open_remote_project_with_mock_connection(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let executor = cx.executor();

        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);

        let remote_fs = FakeFs::new(server_cx.executor());
        remote_fs
            .insert_tree(
                path!("/project"),
                json!({
                    "src": {
                        "main.rs": "fn main() {}",
                    },
                    "README.md": "# Test Project",
                }),
            )
            .await;

        server_cx.update(HeadlessProject::init);
        let http_client = Arc::new(BlockedHttpClient);
        let node_runtime = NodeRuntime::unavailable();
        let languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
        let proxy = Arc::new(ExtensionHostProxy::new());

        let _headless = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session: server_session,
                    fs: remote_fs.clone(),
                    http_client,
                    node_runtime,
                    languages,
                    extension_host_proxy: proxy,
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });

        drop(connect_guard);

        let paths = vec![PathBuf::from(path!("/project"))];
        let open_options = workspace::OpenOptions::default();

        let mut async_cx = cx.to_async();
        let result = open_remote_project(opts, paths, app_state, open_options, &mut async_cx).await;

        executor.run_until_parked();

        assert!(result.is_ok(), "open_remote_project should succeed");

        let windows = cx.update(|cx| cx.windows().len());
        assert_eq!(windows, 1, "Should have opened a window");

        let multi_workspace_handle =
            cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());

        multi_workspace_handle
            .update(cx, |multi_workspace, _, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    let project = workspace.project().read(cx);
                    assert!(project.is_remote(), "Project should be a remote project");
                });
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_reuse_existing_remote_workspace_window(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let executor = cx.executor();

        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);

        let remote_fs = FakeFs::new(server_cx.executor());
        remote_fs
            .insert_tree(
                path!("/project"),
                json!({
                    "src": {
                        "main.rs": "fn main() {}",
                        "lib.rs": "pub fn hello() {}",
                    },
                    "README.md": "# Test Project",
                }),
            )
            .await;

        server_cx.update(HeadlessProject::init);
        let http_client = Arc::new(BlockedHttpClient);
        let node_runtime = NodeRuntime::unavailable();
        let languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
        let proxy = Arc::new(ExtensionHostProxy::new());

        let _headless = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session: server_session,
                    fs: remote_fs.clone(),
                    http_client,
                    node_runtime,
                    languages,
                    extension_host_proxy: proxy,
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });

        drop(connect_guard);

        // First open: create a new window for the remote project.
        let paths = vec![PathBuf::from(path!("/project"))];
        let mut async_cx = cx.to_async();
        open_remote_project(
            opts.clone(),
            paths,
            app_state.clone(),
            workspace::OpenOptions::default(),
            &mut async_cx,
        )
        .await
        .expect("first open_remote_project should succeed");

        executor.run_until_parked();

        assert_eq!(
            cx.update(|cx| cx.windows().len()),
            1,
            "First open should create exactly one window"
        );

        let first_window = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());

        // Verify find_existing_workspace discovers the remote workspace.
        let search_paths = vec![PathBuf::from(path!("/project/src/lib.rs"))];
        let (found, _open_visible) = find_existing_workspace(
            &search_paths,
            &workspace::OpenOptions::default(),
            &SerializedWorkspaceLocation::Remote(opts.clone()),
            &mut async_cx,
        )
        .await;

        assert!(
            found.is_some(),
            "find_existing_workspace should locate the existing remote workspace"
        );
        let (found_window, _found_workspace) = found.unwrap();
        assert_eq!(
            found_window, first_window,
            "find_existing_workspace should return the same window"
        );

        // Second open with the same connection options should reuse the window.
        let second_paths = vec![PathBuf::from(path!("/project/src/lib.rs"))];
        open_remote_project(
            opts.clone(),
            second_paths,
            app_state.clone(),
            workspace::OpenOptions::default(),
            &mut async_cx,
        )
        .await
        .expect("second open_remote_project should succeed via reuse");

        executor.run_until_parked();

        assert_eq!(
            cx.update(|cx| cx.windows().len()),
            1,
            "Second open should reuse the existing window, not create a new one"
        );

        let still_first_window =
            cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());
        assert_eq!(
            still_first_window, first_window,
            "The window handle should be the same after reuse"
        );
    }

    #[gpui::test]
    async fn test_find_existing_remote_workspace_respects_open_new_workspace(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let executor = cx.executor();

        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);

        let remote_fs = FakeFs::new(server_cx.executor());
        remote_fs
            .insert_tree(
                path!("/project"),
                json!({
                    "src": {
                        "main.rs": "fn main() {}",
                        "lib.rs": "pub fn hello() {}",
                    },
                }),
            )
            .await;

        server_cx.update(HeadlessProject::init);
        let http_client = Arc::new(BlockedHttpClient);
        let node_runtime = NodeRuntime::unavailable();
        let languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
        let proxy = Arc::new(ExtensionHostProxy::new());

        let _headless = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session: server_session,
                    fs: remote_fs.clone(),
                    http_client,
                    node_runtime,
                    languages,
                    extension_host_proxy: proxy,
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });

        drop(connect_guard);

        let paths = vec![PathBuf::from(path!("/project"))];
        let mut async_cx = cx.to_async();
        open_remote_project(
            opts.clone(),
            paths,
            app_state,
            workspace::OpenOptions::default(),
            &mut async_cx,
        )
        .await
        .expect("initial open should succeed");

        executor.run_until_parked();

        let search_paths = vec![PathBuf::from(path!("/project/src/lib.rs"))];
        let (found, _open_visible) = find_existing_workspace(
            &search_paths,
            &workspace::OpenOptions {
                open_new_workspace: Some(true),
                ..Default::default()
            },
            &SerializedWorkspaceLocation::Remote(opts),
            &mut async_cx,
        )
        .await;

        assert!(
            found.is_none(),
            "open_new_workspace should suppress remote workspace reuse"
        );
    }

    #[gpui::test]
    async fn test_reconnect_when_server_not_running(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let executor = cx.executor();

        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);

        let remote_fs = FakeFs::new(server_cx.executor());
        remote_fs
            .insert_tree(
                path!("/project"),
                json!({
                    "src": {
                        "main.rs": "fn main() {}",
                    },
                }),
            )
            .await;

        server_cx.update(HeadlessProject::init);
        let http_client = Arc::new(BlockedHttpClient);
        let node_runtime = NodeRuntime::unavailable();
        let languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
        let proxy = Arc::new(ExtensionHostProxy::new());

        let _headless = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session: server_session,
                    fs: remote_fs.clone(),
                    http_client: http_client.clone(),
                    node_runtime: node_runtime.clone(),
                    languages: languages.clone(),
                    extension_host_proxy: proxy.clone(),
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });

        drop(connect_guard);

        // Open the remote project normally.
        let paths = vec![PathBuf::from(path!("/project"))];
        let mut async_cx = cx.to_async();
        open_remote_project(
            opts.clone(),
            paths.clone(),
            app_state.clone(),
            workspace::OpenOptions::default(),
            &mut async_cx,
        )
        .await
        .expect("initial open should succeed");

        executor.run_until_parked();

        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        let window = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());

        // Force the remote client into ServerNotRunning state (simulates the
        // scenario where the remote server died and reconnection failed).
        window
            .update(cx, |multi_workspace, _, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    let client = workspace
                        .project()
                        .read(cx)
                        .remote_client()
                        .expect("should have remote client");
                    client.update(cx, |client, cx| {
                        client.force_server_not_running(cx);
                    });
                });
            })
            .unwrap();

        executor.run_until_parked();

        // Register a new mock server under the same options so the reconnect
        // path can establish a fresh connection.
        let (server_session_2, connect_guard_2) =
            RemoteClient::fake_server_with_opts(&opts, cx, server_cx);

        let _headless_2 = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session: server_session_2,
                    fs: remote_fs.clone(),
                    http_client,
                    node_runtime,
                    languages,
                    extension_host_proxy: proxy,
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });

        drop(connect_guard_2);

        // Simulate clicking "Reconnect": calls open_remote_project with
        // replace_window pointing to the existing window.
        let result = open_remote_project(
            opts,
            paths,
            app_state,
            workspace::OpenOptions {
                requesting_window: Some(window),
                ..Default::default()
            },
            &mut async_cx,
        )
        .await;

        executor.run_until_parked();

        assert!(
            result.is_ok(),
            "reconnect should succeed but got: {:?}",
            result.err()
        );

        // Should still be a single window with a working remote project.
        assert_eq!(cx.update(|cx| cx.windows().len()), 1);

        window
            .update(cx, |multi_workspace, _, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    assert!(
                        workspace.project().read(cx).is_remote(),
                        "project should be remote after reconnect"
                    );
                });
            })
            .unwrap();
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }
}
