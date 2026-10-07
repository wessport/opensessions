use crate::app::{App, Modal, PanelFocus};
use crate::renderer::{THEME_NAMES, compute_hit_target, detail_separator_row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKey {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Tab { shift: bool },
    Enter,
    Esc,
    Backspace,
    CtrlJ,
    CtrlK,
    AltUp,
    AltDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMouse {
    ScrollUp {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    ScrollDown {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Click {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Move {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Drag {
        y: u16,
    },
    DragEnd,
}

pub fn apply_ui_key(app: &mut App, key: UiKey) {
    if app.is_modal_open() {
        apply_modal_key(app, key);
        return;
    }

    match key {
        UiKey::AltUp => app.reorder_focused_session(-1),
        UiKey::AltDown => app.reorder_focused_session(1),
        UiKey::CtrlJ => app.focus_agents_panel(),
        UiKey::CtrlK => app.focus_sessions_panel(),
        UiKey::Down => {
            if app.panel_focus == PanelFocus::Agents {
                app.move_agent_focus(1);
            } else {
                app.move_focus(1);
            }
        }
        UiKey::Up => {
            if app.panel_focus == PanelFocus::Agents {
                app.move_agent_focus(-1);
            } else {
                app.move_focus(-1);
            }
        }
        UiKey::Left => {
            if app.panel_focus == PanelFocus::Sessions {
                app.resize_detail_panel(-1);
            } else {
                app.focus_sessions_panel();
            }
        }
        UiKey::Right => {
            if app.panel_focus == PanelFocus::Sessions {
                let agent_count = app
                    .focused_session_name()
                    .and_then(|name| app.sessions.iter().find(|s| s.name == name))
                    .map(|s| s.agents.len())
                    .unwrap_or(0);
                if agent_count > 0 {
                    app.focus_agents_panel();
                } else {
                    app.resize_detail_panel(1);
                }
            }
        }
        UiKey::Tab { shift } => app.handle_tab(shift),
        UiKey::Enter => app.activate_focused_item(),
        UiKey::Esc => app.focus_sessions_panel(),
        UiKey::Backspace => {}
        UiKey::Char(ch) => app.handle_key_char(ch),
    }
}

fn apply_modal_key(app: &mut App, key: UiKey) {
    match &app.modal {
        Modal::RenameSession { .. } => apply_rename_session_key(app, key),
        Modal::ThemePicker { .. } => apply_theme_picker_key(app, key),
        Modal::WidthSlider { .. } => apply_width_slider_key(app, key),
        Modal::KillConfirm { .. } => apply_kill_confirm_key(app, key),
        Modal::QuitConfirm => apply_quit_confirm_key(app, key),
        Modal::WindowManager { .. } => apply_window_manager_key(app, key),
        Modal::None => {}
    }
}

fn apply_rename_session_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Esc => app.modal = Modal::None,
        UiKey::Enter => app.confirm_rename_session(),
        UiKey::Backspace => {
            if let Modal::RenameSession { draft, .. } = &mut app.modal {
                draft.pop();
            }
        }
        UiKey::Char(ch) => {
            if let Modal::RenameSession { draft, .. } = &mut app.modal {
                draft.push(ch);
            }
        }
        _ => {}
    }
}

fn apply_window_manager_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Up => app.move_window_manager_selection(-1),
        UiKey::Down => app.move_window_manager_selection(1),
        UiKey::Right => app.set_selected_window_marked(true),
        UiKey::Left => app.set_selected_window_marked(false),
        UiKey::Enter => app.activate_or_advance_window_manager(),
        UiKey::Esc => app.back_or_close_window_manager(),
        _ => {}
    }
}

fn filtered_theme_names(query: &str) -> Vec<&'static str> {
    let query_lower = query.to_lowercase();
    THEME_NAMES
        .iter()
        .copied()
        .filter(|name| query_lower.is_empty() || name.contains(&query_lower))
        .collect()
}

fn apply_theme_picker_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Esc => {
            app.close_theme_picker();
        }
        UiKey::Enter => {
            app.confirm_theme_picker();
        }
        UiKey::Left => app.set_transparent_background(true),
        UiKey::Right => app.set_transparent_background(false),
        UiKey::Up => {
            if let Modal::ThemePicker {
                query, selected, ..
            } = &mut app.modal
            {
                let names = filtered_theme_names(query);
                if !names.is_empty() && *selected > 0 {
                    *selected -= 1;
                    app.theme = Some(names[*selected].to_string());
                }
            }
        }
        UiKey::Down => {
            if let Modal::ThemePicker {
                query, selected, ..
            } = &mut app.modal
            {
                let names = filtered_theme_names(query);
                if !names.is_empty() && *selected + 1 < names.len() {
                    *selected += 1;
                    app.theme = Some(names[*selected].to_string());
                }
            }
        }
        UiKey::Backspace => {
            if let Modal::ThemePicker {
                query, selected, ..
            } = &mut app.modal
            {
                query.pop();
                let names = filtered_theme_names(query);
                *selected = (*selected).min(names.len().saturating_sub(1));
                if let Some(name) = names.get(*selected) {
                    app.theme = Some(name.to_string());
                }
            }
        }
        UiKey::Char(ch) => {
            if let Modal::ThemePicker {
                query, selected, ..
            } = &mut app.modal
            {
                query.push(ch);
                let names = filtered_theme_names(query);
                *selected = 0;
                if let Some(name) = names.first() {
                    app.theme = Some(name.to_string());
                }
            }
        }
        _ => {}
    }
}

fn apply_kill_confirm_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Char('y' | 'Y') => {
            if matches!(app.modal, Modal::KillConfirm { .. }) {
                app.confirm_kill_target();
            }
        }
        _ => {
            app.modal = Modal::None;
        }
    }
}

fn apply_quit_confirm_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Char('y' | 'Y') => app.confirm_quit(),
        _ => app.modal = Modal::None,
    }
}

fn apply_width_slider_key(app: &mut App, key: UiKey) {
    match key {
        UiKey::Left | UiKey::Down => app.adjust_width_slider(-1),
        UiKey::Right | UiKey::Up => app.adjust_width_slider(1),
        UiKey::Char('h') => app.adjust_width_slider(-1),
        UiKey::Char('l') => app.adjust_width_slider(1),
        UiKey::Char('H') => app.adjust_width_slider(-5),
        UiKey::Char('L') => app.adjust_width_slider(5),
        UiKey::Enter => app.confirm_width_slider(),
        UiKey::Esc => app.close_width_slider(),
        _ => {}
    }
}

pub fn apply_ui_mouse(app: &mut App, event: UiMouse) {
    if app.is_modal_open() {
        apply_modal_mouse(app, event);
        return;
    }

    match event {
        UiMouse::ScrollUp {
            x: _,
            y,
            width,
            height,
        } => {
            let separator_row = detail_separator_row(app, width, height);
            let session_rows = separator_row.saturating_sub(3) as usize;
            if y < separator_row {
                app.scroll_sessions(-1, session_rows);
            } else if app.panel_focus == PanelFocus::Agents {
                app.move_agent_focus(-1);
            } else {
                app.scroll_sessions(-1, session_rows);
            }
        }
        UiMouse::ScrollDown {
            x: _,
            y,
            width,
            height,
        } => {
            let separator_row = detail_separator_row(app, width, height);
            let session_rows = separator_row.saturating_sub(3) as usize;
            if y < separator_row {
                app.scroll_sessions(1, session_rows);
            } else if app.panel_focus == PanelFocus::Agents {
                app.move_agent_focus(1);
            } else {
                app.scroll_sessions(1, session_rows);
            }
        }
        UiMouse::Click {
            x,
            y,
            width,
            height,
        } => {
            // Check if clicking on the separator row to start a drag resize
            if y == detail_separator_row(app, width, height) {
                app.resize_drag_state = Some((y, app.detail_panel_height));
                return;
            }

            let target = compute_hit_target(app, x, y, width, height);
            if let Some(target) = target {
                app.activate_hit_target(target);
            }
        }
        UiMouse::Move {
            x,
            y,
            width,
            height,
        } => {
            app.set_hover_target(compute_hit_target(app, x, y, width, height));
        }
        UiMouse::Drag { y } => {
            if let Some((start_y, start_height)) = app.resize_drag_state {
                let delta = start_y as i16 - y as i16;
                let new_height = (start_height as i16 + delta).max(4) as usize;
                app.set_detail_panel_height(new_height);
            }
        }
        UiMouse::DragEnd => {
            app.resize_drag_state = None;
        }
    }
}

/// Modals own input, so the sidebar behind them never sees the mouse. A
/// click cancels a y/n confirmation, matching "any key but y cancels";
/// scrolling and hovering are ignored so they cannot dismiss it by accident.
/// Editing modals (rename, theme, width, windows) ignore the mouse and keep
/// their keyboard Esc/Enter semantics.
fn apply_modal_mouse(app: &mut App, event: UiMouse) {
    match event {
        UiMouse::Click { .. }
            if matches!(app.modal, Modal::QuitConfirm | Modal::KillConfirm { .. }) =>
        {
            app.modal = Modal::None;
        }
        UiMouse::DragEnd => app.resize_drag_state = None,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Modal;
    use crate::generated::protocol::{
        AgentPanelScope, ClientCommand, ServerMessage, ServerState, SessionData, WindowData,
    };

    fn session_data(name: &str) -> SessionData {
        SessionData {
            name: name.to_string(),
            created_at: 0,
            dir: format!("/tmp/{name}"),
            branch: String::new(),
            dirty: false,
            changed_files: 0,
            insertions: 0,
            deletions: 0,
            is_worktree: false,
            unseen: false,
            panes: 3,
            ports: Vec::new(),
            local_links: Vec::new(),
            windows: 3,
            uptime: String::new(),
            agent_state: None,
            agents: Vec::new(),
            event_timestamps: Vec::new(),
            metadata: None,
        }
    }

    fn app_with_sessions(names: &[&str]) -> App {
        App::from_state(ServerState {
            sessions: names.iter().map(|name| session_data(name)).collect(),
            focused_session: Some(names[0].to_string()),
            current_session: Some(names[0].to_string()),
            visible_sidebar_pane_ids: Vec::new(),
            theme: None,
            transparent_background: false,
            session_filter: None,
            agent_panel_scope: AgentPanelScope::Current,
            sidebar_width: 36,
            detail_panel_height: 10,
            settings_revision: 0,
            initializing: false,
            init_label: None,
            collapsed_worktree_groups: Vec::new(),
            ts: 0,
        })
    }

    fn app_with_windows() -> App {
        let mut app = App::from_state(ServerState {
            sessions: vec![session_data("project")],
            focused_session: Some("project".to_string()),
            current_session: Some("project".to_string()),
            visible_sidebar_pane_ids: Vec::new(),
            theme: None,
            transparent_background: false,
            session_filter: None,
            agent_panel_scope: AgentPanelScope::Current,
            sidebar_width: 36,
            detail_panel_height: 10,
            settings_revision: 0,
            initializing: false,
            init_label: None,
            collapsed_worktree_groups: Vec::new(),
            ts: 0,
        });
        apply_ui_key(&mut app, UiKey::Char('W'));
        assert_eq!(
            app.drain_commands(),
            vec![ClientCommand::RequestWindows {
                session: "project".to_string()
            }]
        );
        app.apply_server_message(ServerMessage::WindowList {
            session: "project".to_string(),
            windows: vec![
                WindowData {
                    id: "@1".to_string(),
                    index: 0,
                    name: "active".to_string(),
                    active: true,
                    pane_commands: vec!["amp".to_string()],
                },
                WindowData {
                    id: "@2".to_string(),
                    index: 1,
                    name: "regression".to_string(),
                    active: false,
                    pane_commands: vec!["zsh".to_string()],
                },
                WindowData {
                    id: "@3".to_string(),
                    index: 2,
                    name: "ssh".to_string(),
                    active: false,
                    pane_commands: vec!["ssh".to_string()],
                },
            ],
        });
        app
    }

    #[test]
    fn theme_picker_toggles_background_independently_from_palette() {
        let mut app = app_with_windows();
        app.theme = Some("electric-fusion".to_string());
        app.open_theme_picker();

        apply_ui_key(&mut app, UiKey::Left);
        assert!(app.transparent_background);
        apply_ui_key(&mut app, UiKey::Right);
        assert!(!app.transparent_background);
        apply_ui_key(&mut app, UiKey::Left);
        apply_ui_key(&mut app, UiKey::Enter);

        assert_eq!(app.theme.as_deref(), Some("electric-fusion"));
        assert!(app.transparent_background);
        assert_eq!(
            app.drain_commands(),
            vec![ClientCommand::SetTheme {
                theme: "electric-fusion".to_string(),
                transparent_background: true,
                request_id: 1,
            }]
        );
    }

    #[test]
    fn rename_dialog_submits_the_edited_session_name() {
        let mut app = app_with_windows();
        app.modal = Modal::RenameSession {
            original_name: "project".to_string(),
            draft: "renamed-project".to_string(),
        };

        apply_ui_key(&mut app, UiKey::Enter);

        assert_eq!(app.modal, Modal::None);
        assert_eq!(
            app.drain_commands(),
            vec![ClientCommand::RenameSession {
                name: "project".to_string(),
                new_name: "renamed-project".to_string(),
            }]
        );
    }

    #[test]
    fn enter_switches_to_highlighted_window_when_nothing_is_marked() {
        let mut app = app_with_windows();

        apply_ui_key(&mut app, UiKey::Down);
        apply_ui_key(&mut app, UiKey::Enter);

        assert_eq!(app.modal, Modal::None);
        assert_eq!(
            app.drain_commands(),
            vec![ClientCommand::SwitchWindow {
                session: "project".to_string(),
                window_id: "@2".to_string(),
            }]
        );
    }

    #[test]
    fn arrows_mark_only_inactive_windows_and_enter_requires_confirmation() {
        let mut app = app_with_windows();

        apply_ui_key(&mut app, UiKey::Right);
        apply_ui_key(&mut app, UiKey::Down);
        apply_ui_key(&mut app, UiKey::Right);
        apply_ui_key(&mut app, UiKey::Down);
        apply_ui_key(&mut app, UiKey::Right);
        apply_ui_key(&mut app, UiKey::Enter);

        assert!(matches!(
            app.modal,
            Modal::WindowManager {
                confirming: true,
                ref marked,
                ..
            } if marked.len() == 2 && !marked.contains("@1")
        ));
        assert!(app.drain_commands().is_empty());

        apply_ui_key(&mut app, UiKey::Enter);

        assert_eq!(app.modal, Modal::None);
        assert_eq!(
            app.drain_commands(),
            vec![ClientCommand::KillWindows {
                session: "project".to_string(),
                window_ids: vec!["@2".to_string(), "@3".to_string()],
            }]
        );
    }

    #[test]
    fn escape_returns_from_confirmation_before_closing_window_manager() {
        let mut app = app_with_windows();
        apply_ui_key(&mut app, UiKey::Down);
        apply_ui_key(&mut app, UiKey::Right);
        apply_ui_key(&mut app, UiKey::Enter);

        apply_ui_key(&mut app, UiKey::Esc);
        assert!(matches!(
            app.modal,
            Modal::WindowManager {
                confirming: false,
                ..
            }
        ));

        apply_ui_key(&mut app, UiKey::Esc);
        assert_eq!(app.modal, Modal::None);
    }

    #[test]
    fn q_asks_before_quitting_and_y_quits() {
        let mut app = app_with_windows();
        app.modal = Modal::None;

        apply_ui_key(&mut app, UiKey::Char('q'));
        assert_eq!(app.modal, Modal::QuitConfirm);
        assert!(app.drain_commands().is_empty());
        assert!(app.quit_deadline.is_none());

        apply_ui_key(&mut app, UiKey::Char('y'));
        assert_eq!(app.modal, Modal::None);
        assert_eq!(app.drain_commands(), vec![ClientCommand::Quit]);
        assert!(app.quit_deadline.is_some());
    }

    #[test]
    fn any_other_key_cancels_the_quit_confirmation() {
        for key in [UiKey::Char('n'), UiKey::Char('q'), UiKey::Esc, UiKey::Enter] {
            let mut app = app_with_windows();
            app.modal = Modal::None;
            apply_ui_key(&mut app, UiKey::Char('q'));

            apply_ui_key(&mut app, key);

            assert_eq!(app.modal, Modal::None, "{key:?} should cancel");
            assert!(app.drain_commands().is_empty(), "{key:?} must not quit");
            assert!(app.quit_deadline.is_none(), "{key:?} must not arm quit");
        }
    }

    const W: u16 = 36;
    const H: u16 = 40;

    /// Row of the `other` session in a two-session sidebar with no modal.
    fn other_session_row(app: &App) -> u16 {
        crate::renderer::compute_hit_map(app, W, H)
            .iter()
            .position(|hit| *hit == Some(crate::renderer::HitTarget::Session("other".to_string())))
            .expect("other session row") as u16
    }

    fn every_mouse_event(app: &App, session_row: u16) -> Vec<UiMouse> {
        let separator = detail_separator_row(app, W, H);
        vec![
            UiMouse::Click {
                x: 2,
                y: session_row,
                width: W,
                height: H,
            },
            UiMouse::Click {
                x: 2,
                y: separator,
                width: W,
                height: H,
            },
            UiMouse::ScrollDown {
                x: 2,
                y: session_row,
                width: W,
                height: H,
            },
            UiMouse::ScrollUp {
                x: 2,
                y: H - 4,
                width: W,
                height: H,
            },
            UiMouse::Move {
                x: 2,
                y: session_row,
                width: W,
                height: H,
            },
            UiMouse::Drag {
                y: separator.saturating_sub(5),
            },
            UiMouse::DragEnd,
        ]
    }

    #[test]
    fn mouse_input_does_not_reach_the_sidebar_behind_an_editing_modal() {
        let base = app_with_sessions(&["project", "other"]);
        let row = other_session_row(&base);
        let modals = [
            Modal::RenameSession {
                original_name: "project".to_string(),
                draft: "project".to_string(),
            },
            Modal::ThemePicker {
                query: String::new(),
                selected: 0,
                original_theme: None,
                original_transparent_background: false,
            },
            Modal::WidthSlider { draft_width: 36 },
            Modal::WindowManager {
                session: "project".to_string(),
                windows: Vec::new(),
                selected: 0,
                marked: Default::default(),
                confirming: false,
            },
        ];
        for modal in modals {
            for event in every_mouse_event(&base, row) {
                let mut app = app_with_sessions(&["project", "other"]);
                app.modal = modal.clone();

                apply_ui_mouse(&mut app, event);

                assert_eq!(app.modal, modal, "{event:?} must not change the modal");
                assert!(
                    app.drain_commands().is_empty(),
                    "{event:?} leaked to the sidebar"
                );
                assert!(
                    app.drain_launches().is_empty(),
                    "{event:?} launched behind the modal"
                );
                assert_eq!(
                    app.resize_drag_state, None,
                    "{event:?} started a panel drag"
                );
                assert_eq!(app.detail_panel_height, 10, "{event:?} resized the panel");
                assert_eq!(app.flash_target, None, "{event:?} flashed a row");
                assert_eq!(app.hover_target, None, "{event:?} hovered a row");
                assert_eq!(app.focused_session_name(), Some("project"));
            }
        }
    }

    #[test]
    fn a_click_cancels_a_confirmation_without_activating_what_is_under_it() {
        let base = app_with_sessions(&["project", "other"]);
        let row = other_session_row(&base);
        let confirmations = [
            Modal::QuitConfirm,
            Modal::KillConfirm {
                target: crate::app::KillTarget::Session("project".to_string()),
            },
        ];
        for modal in confirmations {
            for event in every_mouse_event(&base, row) {
                let mut app = app_with_sessions(&["project", "other"]);
                app.modal = modal.clone();

                apply_ui_mouse(&mut app, event);

                if matches!(event, UiMouse::Click { .. }) {
                    assert_eq!(app.modal, Modal::None, "a click should cancel {modal:?}");
                } else {
                    assert_eq!(app.modal, modal, "{event:?} must not answer {modal:?}");
                }
                assert!(
                    app.drain_commands().is_empty(),
                    "{event:?} leaked to the sidebar"
                );
                assert!(app.quit_deadline.is_none(), "{event:?} must not arm quit");
                assert_eq!(
                    app.resize_drag_state, None,
                    "{event:?} started a panel drag"
                );
                assert_eq!(app.flash_target, None, "{event:?} flashed a row");
                assert_eq!(app.focused_session_name(), Some("project"));
            }
        }
    }

    #[test]
    fn uppercase_y_confirms_quit_and_kill_like_lowercase() {
        let mut app = app_with_sessions(&["project", "other"]);
        apply_ui_key(&mut app, UiKey::Char('q'));
        apply_ui_key(&mut app, UiKey::Char('Y'));
        assert_eq!(app.modal, Modal::None);
        assert_eq!(app.drain_commands(), vec![ClientCommand::Quit]);

        let mut app = app_with_sessions(&["project", "other"]);
        app.modal = Modal::KillConfirm {
            target: crate::app::KillTarget::Session("other".to_string()),
        };
        apply_ui_key(&mut app, UiKey::Char('Y'));
        assert_eq!(app.modal, Modal::None);
        assert!(matches!(
            app.drain_commands().as_slice(),
            [ClientCommand::KillSession { name, .. }] if name == "other"
        ));
    }
}
