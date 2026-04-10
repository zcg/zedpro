use super::*;
use fs::FakeFs;
use gpui::TestAppContext;
use project::{DisableAiSettings, ProjectGroupKey};
use serde_json::json;
use settings::SettingsStore;
use std::path::Path;

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        DisableAiSettings::register(cx);
    });
}

#[gpui::test]
async fn test_sidebar_disabled_when_disable_ai_is_enabled(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    multi_workspace.read_with(cx, |mw, cx| {
        assert!(mw.multi_workspace_enabled(cx));
    });

    multi_workspace.update_in(cx, |mw, _window, cx| {
        mw.open_sidebar(cx);
        assert!(mw.sidebar_open());
    });

    cx.update(|_window, cx| {
        DisableAiSettings::override_global(DisableAiSettings { disable_ai: true }, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, cx| {
        assert!(
            !mw.sidebar_open(),
            "Sidebar should be closed when disable_ai is true"
        );
        assert!(
            !mw.multi_workspace_enabled(cx),
            "Multi-workspace should be disabled when disable_ai is true"
        );
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    multi_workspace.read_with(cx, |mw, _cx| {
        assert!(
            !mw.sidebar_open(),
            "Sidebar should remain closed when toggled with disable_ai true"
        );
    });

    cx.update(|_window, cx| {
        DisableAiSettings::override_global(DisableAiSettings { disable_ai: false }, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, cx| {
        assert!(
            mw.multi_workspace_enabled(cx),
            "Multi-workspace should be enabled after re-enabling AI"
        );
        assert!(
            !mw.sidebar_open(),
            "Sidebar should still be closed after re-enabling AI (not auto-opened)"
        );
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    multi_workspace.read_with(cx, |mw, _cx| {
        assert!(
            mw.sidebar_open(),
            "Sidebar should open when toggled after re-enabling AI"
        );
    });
}

#[gpui::test]
async fn test_project_group_keys_initial(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let expected_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<&ProjectGroupKey> = mw.project_group_keys().collect();
        assert_eq!(keys.len(), 1, "should have exactly one key on creation");
        assert_eq!(*keys[0], expected_key);
    });
}

#[gpui::test]
async fn test_project_group_keys_add_workspace(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs.clone(), ["/root_b".as_ref()], cx).await;

    let key_a = project_a.read_with(cx, |p, cx| p.project_group_key(cx));
    let key_b = project_b.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_ne!(
        key_a, key_b,
        "different roots should produce different keys"
    );

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(mw.project_group_keys().count(), 1);
    });

    // Adding a workspace with a different project root adds a new key.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<&ProjectGroupKey> = mw.project_group_keys().collect();
        assert_eq!(
            keys.len(),
            2,
            "should have two keys after adding a second workspace"
        );
        assert_eq!(*keys[0], key_b);
        assert_eq!(*keys[1], key_a);
    });
}

#[gpui::test]
async fn test_project_group_keys_duplicate_not_added(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    // A second project entity pointing at the same path produces the same key.
    let project_a2 = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;

    let key_a = project_a.read_with(cx, |p, cx| p.project_group_key(cx));
    let key_a2 = project_a2.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_eq!(key_a, key_a2, "same root path should produce the same key");

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_a2, window, cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<&ProjectGroupKey> = mw.project_group_keys().collect();
        assert_eq!(
            keys.len(),
            1,
            "duplicate key should not be added when a workspace with the same root is inserted"
        );
    });
}
#[gpui::test]
async fn test_project_group_keys_on_worktree_added(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    let project = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let initial_key = project.read_with(cx, |p, cx| p.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    // Add a second worktree to the same project.
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/root_b", true, cx)
        })
        .await
        .unwrap();
    worktree
        .read_with(cx, |tree, _| tree.as_local().unwrap().scan_complete())
        .await;
    cx.run_until_parked();

    let updated_key = project.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_ne!(
        initial_key, updated_key,
        "key should change after adding a worktree"
    );

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<&ProjectGroupKey> = mw.project_group_keys().collect();
        assert_eq!(
            keys.len(),
            1,
            "changing root paths should update the existing group instead of splitting it"
        );
        assert_eq!(*keys[0], updated_key);
    });
}

#[gpui::test]
async fn test_project_group_keys_on_worktree_removed(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    let project = Project::test(fs, ["/root_a".as_ref(), "/root_b".as_ref()], cx).await;

    let initial_key = project.read_with(cx, |p, cx| p.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    // Remove one worktree.
    let worktree_b_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .find(|wt| wt.read(cx).root_name().as_unix_str() == "root_b")
            .unwrap()
            .read(cx)
            .id()
    });
    project.update(cx, |project, cx| {
        project.remove_worktree(worktree_b_id, cx);
    });
    cx.run_until_parked();

    let updated_key = project.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_ne!(
        initial_key, updated_key,
        "key should change after removing a worktree"
    );

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<&ProjectGroupKey> = mw.project_group_keys().collect();
        assert_eq!(
            keys.len(),
            1,
            "changing root paths should update the existing group instead of splitting it"
        );
        assert_eq!(*keys[0], updated_key);
    });
}

#[gpui::test]
async fn test_project_group_keys_across_multiple_workspaces_and_worktree_changes(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_c", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs.clone(), ["/root_b".as_ref()], cx).await;

    let key_a = project_a.read_with(cx, |p, cx| p.project_group_key(cx));
    let key_b = project_b.read_with(cx, |p, cx| p.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(mw.project_group_keys().count(), 2);
    });

    // Now add a worktree to project_a. This should produce a third key.
    let (worktree, _) = project_a
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/root_c", true, cx)
        })
        .await
        .unwrap();
    worktree
        .read_with(cx, |tree, _| tree.as_local().unwrap().scan_complete())
        .await;
    cx.run_until_parked();

    let key_a_updated = project_a.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_ne!(key_a, key_a_updated);

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<&ProjectGroupKey> = mw.project_group_keys().collect();
        assert_eq!(
            keys.len(),
            2,
            "root path changes should preserve grouping without duplicating the old key"
        );
        assert_eq!(*keys[0], key_a_updated);
        assert_eq!(*keys[1], key_b);
    });
}

#[gpui::test]
async fn test_remove_folder_from_project_group_replaces_old_key(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    let project = Project::test(fs, ["/root_a".as_ref(), "/root_b".as_ref()], cx).await;

    let initial_key = project.read_with(cx, |p, cx| p.project_group_key(cx));
    let expected_key = ProjectGroupKey::new(initial_key.host(), PathList::new(&["/root_a"]));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
        mw.remove_folder_from_project_group(&initial_key, Path::new("/root_b"), cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<&ProjectGroupKey> = mw.project_group_keys().collect();
        assert_eq!(
            keys.len(),
            1,
            "old group key should be removed after editing"
        );
        assert_eq!(*keys[0], expected_key);
        assert_ne!(*keys[0], initial_key);
    });
}

#[gpui::test]
async fn test_add_folders_to_project_group_replaces_old_key(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    let project = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let initial_key = project.read_with(cx, |p, cx| p.project_group_key(cx));
    let expected_key =
        ProjectGroupKey::new(initial_key.host(), PathList::new(&["/root_a", "/root_b"]));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
        mw.add_folders_to_project_group(&initial_key, vec!["/root_b".into()], cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<&ProjectGroupKey> = mw.project_group_keys().collect();
        assert_eq!(
            keys.len(),
            1,
            "old group key should be removed after editing"
        );
        assert_eq!(*keys[0], expected_key);
        assert_ne!(*keys[0], initial_key);
    });
}

#[gpui::test]
async fn test_open_project_add_keeps_active_workspace(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = Project::test(fs, ["/root_a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    let initial_workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    multi_workspace
        .update_in(cx, |mw, window, cx| {
            mw.open_project(vec!["/root_b".into()], OpenMode::Add, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, cx| {
        assert_eq!(mw.workspaces().count(), 2);
        assert_eq!(mw.workspace(), &initial_workspace);
        assert!(
            mw.workspaces()
                .any(|workspace| workspace.read(cx).root_paths(cx)
                    == vec![Path::new("/root_b").into()]),
            "new workspace should be added without stealing activation",
        );
    });
}

#[gpui::test]
async fn test_move_project_group_to_new_window_keeps_all_workspaces(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;

    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs, ["/root_a".as_ref()], cx).await;
    let group_key = project_a.read_with(cx, |p, cx| p.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(mw.workspaces().count(), 2);
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.move_project_group_to_new_window(&group_key, window, cx);
    });
    cx.run_until_parked();

    let windows = cx.windows();
    assert_eq!(
        windows.len(),
        2,
        "moving a project group should open a new window"
    );

    let moved_multi_workspace = windows
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .find_map(|window| {
            let root = window.root(cx).ok()?;
            (root != multi_workspace).then_some(root)
        })
        .expect("new multi-workspace window should exist");

    moved_multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(
            mw.workspaces().count(),
            2,
            "all workspaces in the project group should move together",
        );
    });
}
