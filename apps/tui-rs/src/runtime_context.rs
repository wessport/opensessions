#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneIdentity {
    pub pane_id: String,
    pub session_name: String,
    pub window_id: Option<String>,
}

pub fn pane_identity_from_env<F>(env: F) -> Option<PaneIdentity>
where
    F: Fn(&str) -> Option<String>,
{
    pane_identity_resolve(env, |_, _| None)
}

/// Resolve the running pane's identity, mirroring
/// tmux display-message fallback used by the live sidebar.
///
/// `env` reads process environment variables. `tmux_query` invokes
/// `tmux display-message -p -t <target> <format>` and returns the trimmed
/// stdout. Tmux is only consulted when the corresponding `OPENSESSIONS_*`
/// env vars are absent.
pub fn pane_identity_resolve<F, T>(env: F, tmux_query: T) -> Option<PaneIdentity>
where
    F: Fn(&str) -> Option<String>,
    T: Fn(&str, &str) -> Option<String>,
{
    let pane_id = env("TMUX_PANE")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;

    let session_name = env("OPENSESSIONS_SESSION_NAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            tmux_query("#{session_name}", &pane_id)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })?;

    let window_id = env("OPENSESSIONS_WINDOW_ID")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            tmux_query("#{window_id}", &pane_id)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });

    Some(PaneIdentity {
        pane_id,
        session_name,
        window_id,
    })
}

/// Plan describing which tmux pane should receive focus after the sidebar
/// finishes capability detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefocusPlan {
    pub select_pane: String,
}

/// Refocus-main-pane planning as a pure function.
///
/// `tmux_query` is invoked with the argv slice that should be passed to
/// `tmux <args>`. It must return the trimmed stdout when the command succeeds
/// or `None` when it fails. The function is decoupled from process spawning to
/// keep red/green TDD on the sidebar refocus rules straightforward.
pub fn refocus_plan<F>(
    pane_id: &str,
    refocus_window_env: Option<&str>,
    tmux_query: F,
) -> Option<RefocusPlan>
where
    F: Fn(&[&str]) -> Option<String>,
{
    let window_id = match refocus_window_env.map(str::trim).filter(|s| !s.is_empty()) {
        Some(window) => window.to_string(),
        None => tmux_query(&["display-message", "-t", pane_id, "-p", "#{window_id}"])
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?,
    };

    let panes = tmux_query(&[
        "list-panes",
        "-t",
        &window_id,
        "-F",
        "#{pane_id}\t#{pane_active}",
    ])?;
    let panes = panes
        .lines()
        .filter_map(|line| line.trim().split_once('\t'))
        .collect::<Vec<_>>();
    if !panes
        .iter()
        .any(|(candidate, active)| *candidate == pane_id && *active == "1")
    {
        return None;
    }
    let main_pane = panes
        .iter()
        .map(|(candidate, _)| *candidate)
        .find(|candidate| !candidate.is_empty() && *candidate != pane_id)?;

    Some(RefocusPlan {
        select_pane: main_pane.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refocus_plan_excludes_untitled_sidebar_pane_by_id() {
        let plan = refocus_plan("%1", Some("@2"), |args| {
            assert_eq!(
                args,
                ["list-panes", "-t", "@2", "-F", "#{pane_id}\t#{pane_active}"]
            );
            Some("%1\t1\n%2\t0".to_string())
        });

        assert_eq!(
            plan,
            Some(RefocusPlan {
                select_pane: "%2".to_string(),
            })
        );
    }

    #[test]
    fn refocus_plan_does_not_steal_focus_after_user_leaves_sidebar() {
        let plan = refocus_plan("%1", Some("@2"), |args| {
            assert_eq!(
                args,
                ["list-panes", "-t", "@2", "-F", "#{pane_id}\t#{pane_active}"]
            );
            Some("%1\t0\n%2\t1".to_string())
        });

        assert_eq!(plan, None);
    }
}
