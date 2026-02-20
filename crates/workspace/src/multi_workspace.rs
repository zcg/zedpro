use anyhow::Result;
use feature_flags::{AgentV2FeatureFlag, FeatureFlagAppExt};
use gpui::{
    AnyView, App, Context, DragMoveEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    ManagedView, MouseButton, Pixels, Render, SharedString, Subscription,
    SystemWindowTabController, Task, Tiling, Window, WindowId, actions, deferred, px,
};
use project::Project;
use settings::{Settings, SettingsStore};
use std::{collections::{HashMap, HashSet}, future::Future, path::PathBuf};
use ui::prelude::*;
use util::ResultExt;

const SIDEBAR_RESIZE_HANDLE_SIZE: Pixels = px(6.0);

use crate::{
    DockPosition, Item, ModalView, Panel, Toast, Workspace, WorkspaceId, client_side_decorations,
    notifications::NotificationId,
    WorkspaceSettings,
};

actions!(
    multi_workspace,
    [
        /// Creates a new workspace within the current window.
        NewWorkspaceInWindow,
        /// Switches to the next workspace within the current window.
        NextWorkspaceInWindow,
        /// Switches to the previous workspace within the current window.
        PreviousWorkspaceInWindow,
        /// Toggles the workspace switcher sidebar.
        ToggleWorkspaceSidebar,
        /// Moves focus to or from the workspace sidebar without closing it.
        FocusWorkspaceSidebar,
    ]
);

pub enum SidebarEvent {
    Open,
    Close,
}

pub trait Sidebar: EventEmitter<SidebarEvent> + Focusable + Render + Sized {
    fn width(&self, cx: &App) -> Pixels;
    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>);
    fn has_notifications(&self, cx: &App) -> bool;
}

pub trait SidebarHandle: 'static + Send + Sync {
    fn width(&self, cx: &App) -> Pixels;
    fn set_width(&self, width: Option<Pixels>, cx: &mut App);
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn focus(&self, window: &mut Window, cx: &mut App);
    fn has_notifications(&self, cx: &App) -> bool;
    fn to_any(&self) -> AnyView;
    fn entity_id(&self) -> EntityId;
}

#[derive(Clone)]
pub struct DraggedSidebar;

impl Render for DraggedSidebar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl<T: Sidebar> SidebarHandle for Entity<T> {
    fn width(&self, cx: &App) -> Pixels {
        self.read(cx).width(cx)
    }

    fn set_width(&self, width: Option<Pixels>, cx: &mut App) {
        self.update(cx, |this, cx| this.set_width(width, cx))
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        let handle = self.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn has_notifications(&self, cx: &App) -> bool {
        self.read(cx).has_notifications(cx)
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn entity_id(&self) -> EntityId {
        Entity::entity_id(self)
    }
}

pub struct MultiWorkspace {
    window_id: WindowId,
    workspaces: Vec<Entity<Workspace>>,
    active_workspace_index: usize,
    sidebar: Option<Box<dyn SidebarHandle>>,
    sidebar_open: bool,
    _sidebar_subscription: Option<Subscription>,
    pending_removal_tasks: Vec<Task<()>>,
    _serialize_task: Option<Task<()>>,
    _create_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
pub struct SidebarWorkspaceEntry {
    pub index: usize,
    pub workspace: Option<Entity<Workspace>>,
    pub tab_title: SharedString,
}

impl MultiWorkspace {
    pub fn new(workspace: Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut was_use_system_window_tabs =
            WorkspaceSettings::get_global(cx).use_system_window_tabs;
        let mut was_window_tab_link_mode =
            WorkspaceSettings::get_global(cx).window_tab_link_mode;

        let settings_subscription = cx.observe_global::<SettingsStore>(move |_, cx| {
            let settings = WorkspaceSettings::get_global(cx);
            let use_system_window_tabs = settings.use_system_window_tabs;
            let window_tab_link_mode = settings.window_tab_link_mode;

            if use_system_window_tabs == was_use_system_window_tabs
                && window_tab_link_mode == was_window_tab_link_mode
            {
                return;
            }

            was_use_system_window_tabs = use_system_window_tabs;
            was_window_tab_link_mode = window_tab_link_mode;
            cx.notify();
        });

        let release_subscription = cx.on_release(|this: &mut MultiWorkspace, _cx| {
            if let Some(task) = this._serialize_task.take() {
                task.detach();
            }
            if let Some(task) = this._create_task.take() {
                task.detach();
            }
            for task in std::mem::take(&mut this.pending_removal_tasks) {
                task.detach();
            }
        });
        let quit_subscription = cx.on_app_quit(Self::app_will_quit);
        Self {
            window_id: window.window_handle().window_id(),
            workspaces: vec![workspace],
            active_workspace_index: 0,
            sidebar: None,
            sidebar_open: false,
            _sidebar_subscription: None,
            pending_removal_tasks: Vec::new(),
            _serialize_task: None,
            _create_task: None,
            _subscriptions: vec![release_subscription, quit_subscription, settings_subscription],
        }
    }

    pub fn register_sidebar<T: Sidebar>(
        &mut self,
        sidebar: Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription =
            cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| match event {
                SidebarEvent::Open => this.toggle_sidebar(window, cx),
                SidebarEvent::Close => {
                    this.close_sidebar(window, cx);
                }
            });
        self.sidebar = Some(Box::new(sidebar));
        self._sidebar_subscription = Some(subscription);
    }

    pub fn sidebar(&self) -> Option<&dyn SidebarHandle> {
        self.sidebar.as_deref()
    }

    pub fn sidebar_open(&self) -> bool {
        self.sidebar_open && self.sidebar.is_some()
    }

    pub fn sidebar_has_notifications(&self, cx: &App) -> bool {
        self.sidebar
            .as_ref()
            .map_or(false, |s| s.has_notifications(cx))
    }

    pub fn multi_workspace_enabled(&self, cx: &App) -> bool {
        cx.has_flag::<AgentV2FeatureFlag>()
    }

    pub fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.multi_workspace_enabled(cx) {
            return;
        }

        if self.sidebar_open {
            self.close_sidebar(window, cx);
        } else {
            self.open_sidebar(cx);
            if let Some(sidebar) = &self.sidebar {
                sidebar.focus(window, cx);
            }
        }
    }

    pub fn focus_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.multi_workspace_enabled(cx) {
            return;
        }

        if self.sidebar_open {
            let sidebar_is_focused = self
                .sidebar
                .as_ref()
                .is_some_and(|s| s.focus_handle(cx).contains_focused(window, cx));

            if sidebar_is_focused {
                let pane = self.workspace().read(cx).active_pane().clone();
                let pane_focus = pane.read(cx).focus_handle(cx);
                window.focus(&pane_focus, cx);
            } else if let Some(sidebar) = &self.sidebar {
                sidebar.focus(window, cx);
            }
        } else {
            self.open_sidebar(cx);
            if let Some(sidebar) = &self.sidebar {
                sidebar.focus(window, cx);
            }
        }
    }

    pub fn open_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = true;
        for workspace in &self.workspaces {
            workspace.update(cx, |workspace, cx| {
                workspace.set_workspace_sidebar_open(true, cx);
            });
        }
        self.serialize(cx);
        cx.notify();
    }

    fn close_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = false;
        for workspace in &self.workspaces {
            workspace.update(cx, |workspace, cx| {
                workspace.set_workspace_sidebar_open(false, cx);
            });
        }
        let pane = self.workspace().read(cx).active_pane().clone();
        let pane_focus = pane.read(cx).focus_handle(cx);
        window.focus(&pane_focus, cx);
        self.serialize(cx);
        cx.notify();
    }

    pub fn is_sidebar_open(&self) -> bool {
        self.sidebar_open
    }

    pub fn workspace(&self) -> &Entity<Workspace> {
        &self.workspaces[self.active_workspace_index]
    }

    pub fn workspaces(&self) -> &[Entity<Workspace>] {
        &self.workspaces
    }

    pub fn active_workspace_index(&self) -> usize {
        self.active_workspace_index
    }

    pub fn activate(&mut self, workspace: Entity<Workspace>, cx: &mut Context<Self>) {
        if !self.multi_workspace_enabled(cx) {
            self.workspaces[0] = workspace;
            self.active_workspace_index = 0;
            cx.notify();
            return;
        }

        let old_index = self.active_workspace_index;
        let new_index = self.set_active_workspace(workspace, cx);
        if old_index != new_index {
            self.serialize(cx);
        }
    }

    fn set_active_workspace(
        &mut self,
        workspace: Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> usize {
        let index = self.add_workspace(workspace, cx);
        self.active_workspace_index = index;
        cx.notify();
        index
    }

    /// Adds a workspace to this window without changing which workspace is active.
    /// Returns the index of the workspace (existing or newly inserted).
    pub fn add_workspace(&mut self, workspace: Entity<Workspace>, cx: &mut Context<Self>) -> usize {
        if let Some(index) = self.workspaces.iter().position(|w| *w == workspace) {
            index
        } else {
            if self.sidebar_open {
                workspace.update(cx, |workspace, cx| {
                    workspace.set_workspace_sidebar_open(true, cx);
                });
            }
            self.workspaces.push(workspace);
            cx.notify();
            self.workspaces.len() - 1
        }
    }

    pub fn activate_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(
            index < self.workspaces.len(),
            "workspace index out of bounds"
        );
        self.active_workspace_index = index;
        self.serialize(cx);
        self.focus_active_workspace(window, cx);
        cx.notify();
    }

    pub fn activate_index_with_link_mode(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.should_link_window_tabs(cx) {
            let current_window_id = window.window_handle().window_id();
            let tab_group_window_ids =
                SystemWindowTabController::tab_group_window_ids(cx, current_window_id);
            let Some(target_window_id) = tab_group_window_ids.get(index).copied() else {
                return;
            };
            if target_window_id == current_window_id {
                return;
            }
            if let Some(target_window_handle) = cx
                .windows()
                .into_iter()
                .find(|window_handle| window_handle.window_id() == target_window_id)
            {
                target_window_handle
                    .update(cx, |_, target_window, _| {
                        target_window.activate_window();
                    })
                    .ok();
            }
            return;
        }

        debug_assert!(
            index < self.workspaces.len(),
            "workspace index out of bounds"
        );
        self.activate_index(index, window, cx);
    }

    pub fn activate_next_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.len() > 1 {
            let next_index = (self.active_workspace_index + 1) % self.workspaces.len();
            self.activate_index(next_index, window, cx);
        }
    }

    pub fn activate_previous_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.len() > 1 {
            let prev_index = if self.active_workspace_index == 0 {
                self.workspaces.len() - 1
            } else {
                self.active_workspace_index - 1
            };
            self.activate_index(prev_index, window, cx);
        }
    }

    pub fn should_link_window_tabs(&self, cx: &App) -> bool {
        let settings = WorkspaceSettings::get_global(cx);
        settings.use_system_window_tabs
            && settings.window_tab_link_mode == settings::WindowTabLinkMode::Linked
    }

    pub fn sidebar_workspace_entries(
        &self,
        current_window_id: WindowId,
        cx: &App,
    ) -> Vec<SidebarWorkspaceEntry> {
        if self.should_link_window_tabs(cx) {
            self.tab_group_workspaces(current_window_id, cx)
        } else {
            self.workspaces
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, workspace)| SidebarWorkspaceEntry {
                    index,
                    workspace: Some(workspace),
                    tab_title: SharedString::new(""),
                })
                .collect::<Vec<_>>()
        }
    }

    pub fn sidebar_active_index(&self, current_window_id: WindowId, cx: &App) -> usize {
        if self.should_link_window_tabs(cx) {
            SystemWindowTabController::tab_group_window_ids(cx, current_window_id)
                .into_iter()
                .position(|window_id| window_id == current_window_id)
                .unwrap_or(0)
        } else {
            self.active_workspace_index
        }
    }

    pub fn remove_sidebar_entry(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.should_link_window_tabs(cx) {
            let current_window_id = window.window_handle().window_id();
            let tab_group_window_ids =
                SystemWindowTabController::tab_group_window_ids(cx, current_window_id);
            if tab_group_window_ids.len() <= 1 {
                return;
            }
            let Some(target_window_id) = tab_group_window_ids.get(index).copied() else {
                return;
            };

            if let Some(target_window_handle) = cx
                .windows()
                .into_iter()
                .find(|window_handle| window_handle.window_id() == target_window_id)
            {
                target_window_handle
                    .update(cx, |_, target_window, _| {
                        target_window.remove_window();
                    })
                    .ok();
            }
            return;
        }

        self.remove_workspace(index, window, cx);
    }

    fn tab_group_workspaces(
        &self,
        current_window_id: WindowId,
        cx: &App,
    ) -> Vec<SidebarWorkspaceEntry> {
        let mut entries = Vec::new();
        let workspace_by_window_id: HashMap<WindowId, Entity<Workspace>> = {
            let app_state = self.workspace().read(cx).app_state().clone();
            app_state
                .workspace_store
                .read(cx)
                .workspaces_with_windows()
                .filter_map(|(window_handle, weak_workspace)| {
                    weak_workspace
                        .upgrade()
                        .map(|workspace| (window_handle.window_id(), workspace))
                })
                .collect()
        };
        let live_window_ids: HashSet<WindowId> = cx
            .windows()
            .into_iter()
            .map(|window_handle| window_handle.window_id())
            .collect();
        let tabs = cx
            .global::<SystemWindowTabController>()
            .tabs(current_window_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|tab| live_window_ids.contains(&tab.id))
            .collect::<Vec<_>>();

        for (tab_index, tab) in tabs.into_iter().enumerate() {
            let workspace = workspace_by_window_id
                .get(&tab.id)
                .cloned()
                .or_else(|| (tab.id == current_window_id).then(|| self.workspace().clone()));
            entries.push(SidebarWorkspaceEntry {
                index: tab_index,
                workspace,
                tab_title: tab.title,
            });
        }

        if entries.is_empty() {
            entries.push(SidebarWorkspaceEntry {
                index: 0,
                workspace: Some(self.workspace().clone()),
                tab_title: SharedString::new(""),
            });
        }

        entries
    }

    fn route_next_workspace_or_window_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let window_id = window.window_handle().window_id();
        if self.should_link_window_tabs(cx) {
            SystemWindowTabController::select_next_tab(cx, window_id);
            return;
        }

        self.activate_next_workspace(window, cx);
    }

    fn route_previous_workspace_or_window_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let window_id = window.window_handle().window_id();
        if self.should_link_window_tabs(cx) {
            SystemWindowTabController::select_previous_tab(cx, window_id);
            return;
        }

        self.activate_previous_workspace(window, cx);
    }

    fn serialize(&mut self, cx: &mut App) {
        let window_id = self.window_id;
        let state = crate::persistence::model::MultiWorkspaceState {
            active_workspace_id: self.workspace().read(cx).database_id(),
            sidebar_open: self.sidebar_open,
        };
        self._serialize_task = Some(cx.background_spawn(async move {
            crate::persistence::write_multi_workspace_state(window_id, state).await;
        }));
    }

    /// Returns the in-flight serialization task (if any) so the caller can
    /// await it. Used by the quit handler to ensure pending DB writes
    /// complete before the process exits.
    pub fn flush_serialization(&mut self) -> Task<()> {
        self._serialize_task.take().unwrap_or(Task::ready(()))
    }

    fn app_will_quit(&mut self, _cx: &mut Context<Self>) -> impl Future<Output = ()> + use<> {
        let mut tasks: Vec<Task<()>> = Vec::new();
        if let Some(task) = self._serialize_task.take() {
            tasks.push(task);
        }
        if let Some(task) = self._create_task.take() {
            tasks.push(task);
        }
        tasks.extend(std::mem::take(&mut self.pending_removal_tasks));

        async move {
            futures::future::join_all(tasks).await;
        }
    }

    fn focus_active_workspace(&self, window: &mut Window, cx: &mut App) {
        let workspace = self.workspace().clone();
        workspace.update(cx, |workspace, cx| {
            workspace.refresh_window_title_and_edited_state(window, cx);
        });

        let focus_handle = {
            // If a dock panel is zoomed, focus it instead of the center pane.
            // Otherwise, focusing the center pane triggers dismiss_zoomed_items_to_reveal
            // which closes the zoomed dock.
            let workspace = workspace.read(cx);
            let mut target = None;
            for dock in workspace.all_docks() {
                let dock = dock.read(cx);
                if dock.is_open() {
                    if let Some(panel) = dock.active_panel() {
                        if panel.is_zoomed(window, cx) {
                            target = Some(panel.panel_focus_handle(cx));
                            break;
                        }
                    }
                }
            }
            target.unwrap_or_else(|| {
                let pane = workspace.active_pane().clone();
                pane.read(cx).focus_handle(cx)
            })
        };
        window.focus(&focus_handle, cx);
    }

    pub fn panel<T: Panel>(&self, cx: &App) -> Option<Entity<T>> {
        self.workspace().read(cx).panel::<T>(cx)
    }

    pub fn active_modal<V: ManagedView + 'static>(&self, cx: &App) -> Option<Entity<V>> {
        self.workspace().read(cx).active_modal::<V>(cx)
    }

    pub fn add_panel<T: Panel>(
        &mut self,
        panel: Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace().update(cx, |workspace, cx| {
            workspace.add_panel(panel, window, cx);
        });
    }

    pub fn focus_panel<T: Panel>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<T>> {
        self.workspace()
            .update(cx, |workspace, cx| workspace.focus_panel::<T>(window, cx))
    }

    pub fn toggle_modal<V: ModalView, B>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: B,
    ) where
        B: FnOnce(&mut Window, &mut gpui::Context<V>) -> V,
    {
        self.workspace().update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, build);
        });
    }

    pub fn toggle_dock(
        &mut self,
        dock_side: DockPosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace().update(cx, |workspace, cx| {
            workspace.toggle_dock(dock_side, window, cx);
        });
    }

    pub fn active_item_as<I: 'static>(&self, cx: &App) -> Option<Entity<I>> {
        self.workspace().read(cx).active_item_as::<I>(cx)
    }

    pub fn items_of_type<'a, T: Item>(
        &'a self,
        cx: &'a App,
    ) -> impl 'a + Iterator<Item = Entity<T>> {
        self.workspace().read(cx).items_of_type::<T>(cx)
    }

    pub fn database_id(&self, cx: &App) -> Option<WorkspaceId> {
        self.workspace().read(cx).database_id()
    }

    pub fn take_pending_removal_tasks(&mut self) -> Vec<Task<()>> {
        let mut tasks: Vec<Task<()>> = std::mem::take(&mut self.pending_removal_tasks)
            .into_iter()
            .filter(|task| !task.is_ready())
            .collect();
        if let Some(task) = self._create_task.take() {
            if !task.is_ready() {
                tasks.push(task);
            }
        }
        tasks
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_random_database_id(&mut self, cx: &mut Context<Self>) {
        self.workspace().update(cx, |workspace, _cx| {
            workspace.set_random_database_id();
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(|cx| Workspace::test_new(project, window, cx));
        Self::new(workspace, window, cx)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_add_workspace(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Workspace> {
        let workspace = cx.new(|cx| Workspace::test_new(project, window, cx));
        self.activate(workspace.clone(), cx);
        workspace
    }

    pub fn create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.multi_workspace_enabled(cx) {
            return;
        }
        let app_state = self.workspace().read(cx).app_state().clone();
        if self.should_link_window_tabs(cx) {
            #[cfg(target_os = "windows")]
            {
                let Some(target_window_handle) = window.window_handle().downcast::<Self>() else {
                    return;
                };
                let create_window_task =
                    Workspace::new_local(Vec::new(), app_state, None, None, None, cx);
                cx.spawn_in(window, async move |_this, cx| {
                    let (source_window_handle, _opened_paths) = create_window_task.await?;

                    let Some(target_window_id) = target_window_handle
                        .update(cx, |_, target_window, _| target_window.window_handle().window_id())
                        .ok()
                    else {
                        return Ok::<(), anyhow::Error>(());
                    };

                    let source_window_id = source_window_handle.window_id();
                    cx.update(|_, cx| {
                        SystemWindowTabController::merge_window_into_group_and_sync_platform(
                            cx,
                            source_window_id,
                            target_window_id,
                            usize::MAX,
                        );
                    })?;

                    source_window_handle
                        .update(cx, |_, source_window, _| {
                            source_window.activate_window();
                        })
                        .ok();
                    target_window_handle
                        .update(cx, |_, _, cx| {
                            cx.notify();
                        })
                        .ok();
                    source_window_handle
                        .update(cx, |_, _, cx| {
                            cx.notify();
                        })
                        .ok();

                    Ok::<(), anyhow::Error>(())
                })
                .detach_and_log_err(cx);
            }
            #[cfg(not(target_os = "windows"))]
            {
                crate::open_new(crate::OpenOptions::default(), app_state, cx, |_, _, _| {})
                    .detach_and_log_err(cx);
            }
            return;
        }

        let project = Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            None,
            project::LocalProjectFlags::default(),
            cx,
        );
        let new_workspace = cx.new(|cx| Workspace::new(None, project, app_state, window, cx));
        self.set_active_workspace(new_workspace.clone(), cx);
        self.focus_active_workspace(window, cx);

        let weak_workspace = new_workspace.downgrade();
        self._create_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = crate::persistence::DB.next_id().await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(workspace_id) => {
                    if let Some(workspace) = weak_workspace.upgrade() {
                        let session_id = workspace.read(cx).session_id();
                        let window_id = window.window_handle().window_id().as_u64();
                        workspace.update(cx, |workspace, _cx| {
                            workspace.set_database_id(workspace_id);
                        });
                        cx.background_spawn(async move {
                            crate::persistence::DB
                                .set_session_binding(workspace_id, session_id, Some(window_id))
                                .await
                                .log_err();
                        })
                        .detach();
                    } else {
                        cx.background_spawn(async move {
                            crate::persistence::DB
                                .delete_workspace_by_id(workspace_id)
                                .await
                                .log_err();
                        })
                        .detach();
                    }
                    this.serialize(cx);
                }
                Err(error) => {
                    log::error!("Failed to create workspace: {error:#}");
                    if let Some(index) = weak_workspace
                        .upgrade()
                        .and_then(|w| this.workspaces.iter().position(|ws| *ws == w))
                    {
                        this.remove_workspace(index, window, cx);
                    }
                    this.workspace().update(cx, |workspace, cx| {
                        let id = NotificationId::unique::<MultiWorkspace>();
                        workspace.show_toast(
                            Toast::new(id, format!("Failed to create workspace: {error}")),
                            cx,
                        );
                    });
                }
            })
            .log_err();
        }));
    }

    pub fn remove_workspace(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.len() <= 1 || index >= self.workspaces.len() {
            return;
        }

        let removed_workspace = self.workspaces.remove(index);

        if self.active_workspace_index >= self.workspaces.len() {
            self.active_workspace_index = self.workspaces.len() - 1;
        } else if self.active_workspace_index > index {
            self.active_workspace_index -= 1;
        }

        if let Some(workspace_id) = removed_workspace.read(cx).database_id() {
            self.pending_removal_tasks.retain(|task| !task.is_ready());
            self.pending_removal_tasks
                .push(cx.background_spawn(async move {
                    crate::persistence::DB
                        .delete_workspace_by_id(workspace_id)
                        .await
                        .log_err();
                }));
        }

        self.serialize(cx);
        self.focus_active_workspace(window, cx);
        cx.notify();
    }

    pub fn open_project(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let workspace = self.workspace().clone();

        if self.should_link_window_tabs(cx) {
            let app_state = workspace.read(cx).app_state().clone();

            #[cfg(target_os = "windows")]
            {
                let Some(target_window_handle) = window.window_handle().downcast::<Self>() else {
                    return workspace.update(cx, |workspace, cx| {
                        workspace.open_workspace_for_paths(true, paths, window, cx)
                    });
                };

                let create_window_task =
                    Workspace::new_local(paths, app_state, None, None, None, cx);
                return cx.spawn_in(window, async move |_this, cx| {
                    let (source_window_handle, _opened_paths) = create_window_task.await?;

                    let Some(target_window_id) = target_window_handle
                        .update(cx, |_, target_window, _| target_window.window_handle().window_id())
                        .ok()
                    else {
                        return Ok(());
                    };

                    let source_window_id = source_window_handle.window_id();
                    cx.update(|_, cx| {
                        SystemWindowTabController::merge_window_into_group_and_sync_platform(
                            cx,
                            source_window_id,
                            target_window_id,
                            usize::MAX,
                        );
                    })?;

                    source_window_handle
                        .update(cx, |_, source_window, _| {
                            source_window.activate_window();
                        })
                        .ok();
                    target_window_handle
                        .update(cx, |_, _, cx| {
                            cx.notify();
                        })
                        .ok();
                    source_window_handle
                        .update(cx, |_, _, cx| {
                            cx.notify();
                        })
                        .ok();

                    Ok(())
                });
            }

            #[cfg(not(target_os = "windows"))]
            {
                let create_window_task =
                    Workspace::new_local(paths, app_state, None, None, None, cx);
                return cx.spawn(async move |_cx| {
                    let _ = create_window_task.await?;
                    Ok(())
                });
            }
        }

        if self.multi_workspace_enabled(cx) {
            workspace.update(cx, |workspace, cx| {
                workspace.open_workspace_for_paths(true, paths, window, cx)
            })
        } else {
            cx.spawn_in(window, async move |_this, cx| {
                let should_continue = workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.prepare_to_close(crate::CloseIntent::ReplaceWindow, window, cx)
                    })?
                    .await?;
                if should_continue {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_workspace_for_paths(true, paths, window, cx)
                        })?
                        .await
                } else {
                    Ok(())
                }
            })
        }
    }
}

impl Render for MultiWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let multi_workspace_enabled = self.multi_workspace_enabled(cx);

        let sidebar: Option<AnyElement> = if multi_workspace_enabled && self.sidebar_open {
            self.sidebar.as_ref().map(|sidebar_handle| {
                let weak = cx.weak_entity();

                let sidebar_width = sidebar_handle.width(cx);
                let resize_handle = deferred(
                    div()
                        .id("sidebar-resize-handle")
                        .absolute()
                        .right(-SIDEBAR_RESIZE_HANDLE_SIZE / 2.)
                        .top(px(0.))
                        .h_full()
                        .w(SIDEBAR_RESIZE_HANDLE_SIZE)
                        .cursor_col_resize()
                        .on_drag(DraggedSidebar, |dragged, _, _, cx| {
                            cx.stop_propagation();
                            cx.new(|_| dragged.clone())
                        })
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_mouse_up(MouseButton::Left, move |event, _, cx| {
                            if event.click_count == 2 {
                                weak.update(cx, |this, cx| {
                                    if let Some(sidebar) = this.sidebar.as_mut() {
                                        sidebar.set_width(None, cx);
                                    }
                                })
                                .ok();
                                cx.stop_propagation();
                            }
                        })
                        .occlude(),
                );

                div()
                    .id("sidebar-container")
                    .relative()
                    .h_full()
                    .w(sidebar_width)
                    .flex_shrink_0()
                    .child(sidebar_handle.to_any())
                    .child(resize_handle)
                    .into_any_element()
            })
        } else {
            None
        };

        client_side_decorations(
            h_flex()
                .key_context("Workspace")
                .size_full()
                .on_action(
                    cx.listener(|this: &mut Self, _: &NewWorkspaceInWindow, window, cx| {
                        this.create_workspace(window, cx);
                    }),
                )
                .on_action(
                    cx.listener(|this: &mut Self, _: &NextWorkspaceInWindow, window, cx| {
                        this.route_next_workspace_or_window_tab(window, cx);
                    }),
                )
                .on_action(cx.listener(
                    |this: &mut Self, _: &PreviousWorkspaceInWindow, window, cx| {
                        this.route_previous_workspace_or_window_tab(window, cx);
                    },
                ))
                .on_action(cx.listener(
                    |this: &mut Self, _: &ToggleWorkspaceSidebar, window, cx| {
                        this.toggle_sidebar(window, cx);
                    },
                ))
                .on_action(
                    cx.listener(|this: &mut Self, _: &FocusWorkspaceSidebar, window, cx| {
                        this.focus_sidebar(window, cx);
                    }),
                )
                .when(
                    self.sidebar_open() && self.multi_workspace_enabled(cx),
                    |this| {
                        this.on_drag_move(cx.listener(
                            |this: &mut Self, e: &DragMoveEvent<DraggedSidebar>, _window, cx| {
                                if let Some(sidebar) = &this.sidebar {
                                    let new_width = e.event.position.x;
                                    sidebar.set_width(Some(new_width), cx);
                                }
                            },
                        ))
                        .children(sidebar)
                    },
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .size_full()
                        .overflow_hidden()
                        .child(self.workspace().clone()),
                ),
            window,
            cx,
            Tiling {
                left: multi_workspace_enabled && self.sidebar_open,
                ..Tiling::default()
            },
        )
    }
}
