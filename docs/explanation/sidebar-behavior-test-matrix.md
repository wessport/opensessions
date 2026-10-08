# Sidebar Behavior E2E Coverage Matrix

This file maps the product contract in [`sidebar-behavior.md`](./sidebar-behavior.md) to the real tmux E2E tests that protect it.

The intended test surface is product E2E only: each test creates real fake git repositories/worktrees, a real isolated tmux server/socket, a real opensessions server, real sidebar panes, and PTY-backed attached tmux clients.

## Covered by `apps/tui-rs/tests/tmux_e2e.rs`

| Behavior | E2E coverage |
| --- | --- |
| Session keyboard focus is local, `Tab` switches, `j`/`k` browse temporary focus, and worktree group focus rehomes to the chosen child session. | `tmux_sidebar_keyboard_focus_and_worktree_flow` |
| Explicit foreground sidebar resize persists once and fans out to every managed sidebar pane in the tmux server. | `tmux_sidebar_width_resize_fans_out_to_every_session_sidebar` |
| `q` then `y` in a connected sidebar shuts down the server and every connected sidebar client. | `tmux_sidebar_quit_closes_the_server_and_every_sidebar_client` |
| `q` in a connected sidebar only asks; any other key cancels and the server and sidebars keep running. | `tmux_sidebar_quit_is_cancelled_without_confirmation` |
| Two attached tmux clients can keep independent active rows instead of a global server focus row overriding every sidebar. | `tmux_sidebar_multiple_clients_keep_independent_active_rows` |
| Two different tmux sockets have isolated ports, servers, width state, and sidebar state, including recorded sidebar visibility. | `tmux_sidebar_state_is_isolated_per_tmux_socket` |
| A restarted server restores visible sidebars in every window, and shutdown does not record the sidebar as hidden. | `tmux_sidebar_restarted_server_restores_visible_sidebars` |
| A sidebar hidden by toggle stays hidden across a server restart, ensure requests, and session switches. | `tmux_sidebar_restarted_server_keeps_hidden_sidebar_hidden` |
| `q` in a normal/main tmux pane does not quit opensessions. | `tmux_sidebar_q_in_main_pane_does_not_quit_opensessions` |
| Pane topology repair must not let tmux permanently donate freed space to the sidebar. | `tmux_sidebar_pane_exit_does_not_steal_sidebar_width` |
| Resizing and immediately switching sessions preserves the latest drag-owned width through handoff. | `tmux_sidebar_resize_immediately_before_switch_survives_handoff` |
| A single resize immediately followed by a switch is adopted from the source window even if no prior drag report established an owner. | `tmux_sidebar_single_resize_immediately_before_switch_is_adopted` |
| Returning to a stale session keeps evenly split content panes, and a proportional repair computed before a window resize is never applied after it. | `tmux_sidebar_preserves_even_content_panes_when_returning_to_stale_session`, `tmux_sidebar_proportional_repair_skips_stale_content_widths_after_window_resize` |
| Shutdown restores each window's original `remain-on-exit` even after sidebar panes have already exited. | `tmux_sidebar_preserves_unrelated_indexed_hooks_across_startup_and_shutdown` |
| A sidebar window with the user's own `remain-on-exit on` keeps both previously dead and newly exited panes. | `tmux_sidebar_pane_death_preserves_an_unrelated_retained_pane` |
| Session switching remains responsive while 100 websocket sidebar clients are connected and state broadcasts are bursting. | `tmux_sidebar_switch_stays_responsive_with_100_connected_clients` |

## Covered by `packages/runtime-rs/tests/tmux_provider_tmux.rs`

These run `TmuxProvider` against a real private tmux server without the opensessions server. Dead-pane tests wait for the `pane-died` hook or, as the server would, the sweep (`close_dead_content_panes`), because tmux 3.4 skips the hook for some pane deaths. Those deaths also have no recorded exit status, so under `remain-on-exit failed` the tests accept such a clean exit being kept, exactly as tmux 3.4 keeps it on its own.

| Behavior | Coverage |
| --- | --- |
| A sidebar window honours the user's `remain-on-exit` (`on` keeps dead panes, `failed` keeps only failed ones) through the hook or the sweep. | `sidebar_windows_keep_dead_panes_when_the_user_wants_them`, `sidebar_windows_honour_remain_on_exit_failed` |
| Without any hook, the sweep alone removes the dead panes the user would not keep, keeps the rest, restores a window whose sidebar is gone, and closes a window left without content after moving its clients to the previously created session. | `the_sweep_alone_*` |
| A pane's death changes the tmux state fingerprint that triggers the server's sweep. | `a_pane_dying_changes_the_tmux_state_fingerprint` |
| The hook script and the sweep make the same decision for every combination of saved `remain-on-exit`, exit status, sidebar/content pane, last content pane, and last window. | unit test `the_sweep_and_the_pane_died_hook_agree_on_every_dead_pane` in `tmux_provider.rs` |

## Important Invariants Covered Indirectly

- Startup/layout-settle resize reports are rejected and repaired back to the coordinator-owned width.
- Background sidebar width reports are treated as echoes, not new user intent.
- A continued drag owner may report a final width after session focus has moved.
- Width fanout skips stale self-fighting during active user drag, then converges all panes.
- Normal websocket clients coalesce broadcast state to latest-wins frame cadence so stale snapshots cannot congest high-priority input.
- E2E tests are serialized inside the process because tmux, PTYs, product binaries, and debug logs are external resources even when each test uses an isolated socket.

## Not Yet Fully Automated

These are still mostly protected by final-state assertions rather than visual frame-by-frame checks:

- no visible intermediate flicker during `Tab` and `Enter` switching
- full terminal resize across multiple actual terminal emulator window sizes
- control-mode client interference
- tmux `window-size latest` restoration after every possible resize path

If one of these regresses, add another product E2E in `tmux_e2e.rs` before changing lower-level code.
