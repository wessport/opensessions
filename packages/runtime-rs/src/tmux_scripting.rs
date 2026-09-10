const SIDEBAR_PANE_TITLE: &str = "opensessions-sidebar";
const SIDEBAR_WIDTH_OPTION: &str = "@opensessions_width";
pub const SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION: &str = "@opensessions_mouse_resize_window";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxVar {
    PaneId,
    PaneTitle,
    PaneWidth,
    WindowPanes,
    SidebarWidthOption,
}

impl TmuxVar {
    fn name(self) -> &'static str {
        match self {
            Self::PaneId => "pane_id",
            Self::PaneTitle => "pane_title",
            Self::PaneWidth => "pane_width",
            Self::WindowPanes => "window_panes",
            Self::SidebarWidthOption => SIDEBAR_WIDTH_OPTION,
        }
    }

    pub fn format(self) -> TmuxFormat {
        TmuxFormat::var(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxFormat {
    Var(TmuxVar),
    VarName(&'static str),
    Literal(String),
    Eq(Box<TmuxFormat>, Box<TmuxFormat>),
    Neq(Box<TmuxFormat>, Box<TmuxFormat>),
    Gt(Box<TmuxFormat>, Box<TmuxFormat>),
    And(Vec<TmuxFormat>),
}

impl TmuxFormat {
    pub fn var(var: TmuxVar) -> Self {
        Self::Var(var)
    }

    pub fn var_name(name: &'static str) -> Self {
        Self::VarName(name)
    }

    pub fn literal(value: impl Into<String>) -> Self {
        Self::Literal(value.into())
    }

    pub fn eq(left: TmuxFormat, right: TmuxFormat) -> Self {
        Self::Eq(Box::new(left), Box::new(right))
    }

    pub fn neq(left: TmuxFormat, right: TmuxFormat) -> Self {
        Self::Neq(Box::new(left), Box::new(right))
    }

    pub fn gt(left: TmuxFormat, right: TmuxFormat) -> Self {
        Self::Gt(Box::new(left), Box::new(right))
    }

    pub fn and(parts: impl IntoIterator<Item = TmuxFormat>) -> Self {
        let mut parts = parts.into_iter().collect::<Vec<_>>();
        if parts.len() == 1 {
            return parts.remove(0);
        }
        Self::And(parts)
    }

    pub fn render(&self) -> String {
        self.render_with_hash("#")
    }

    pub fn render_for_hook(&self) -> String {
        self.render_with_hash("##")
    }

    fn render_with_hash(&self, hash: &str) -> String {
        match self {
            Self::Var(var) => format!("{hash}{{{}}}", var.name()),
            Self::VarName(name) => format!("{hash}{{{name}}}"),
            Self::Literal(value) => value.clone(),
            Self::Eq(left, right) => format!(
                "{hash}{{==:{},{}}}",
                left.render_with_hash(hash),
                right.render_with_hash(hash)
            ),
            Self::Neq(left, right) => format!(
                "{hash}{{!=:{},{}}}",
                left.render_with_hash(hash),
                right.render_with_hash(hash)
            ),
            Self::Gt(left, right) => format!(
                "{hash}{{>:{},{}}}",
                left.render_with_hash(hash),
                right.render_with_hash(hash)
            ),
            Self::And(parts) => render_binary_operator(hash, "&&", parts),
        }
    }
}

fn render_binary_operator(hash: &str, operator: &str, parts: &[TmuxFormat]) -> String {
    match parts {
        [] => String::new(),
        [only] => only.render_with_hash(hash),
        [first, second] => format!(
            "{hash}{{{operator}:{},{}}}",
            first.render_with_hash(hash),
            second.render_with_hash(hash)
        ),
        [first, rest @ ..] => format!(
            "{hash}{{{operator}:{},{}}}",
            first.render_with_hash(hash),
            render_binary_operator(hash, operator, rest)
        ),
    }
}

pub fn hook_context_format() -> &'static str {
    "#{client_tty}|#{session_name}|#{window_id}|#{pane_id}|#{pane_active}"
}

fn hook_context_script() -> String {
    let context = hook_context_format().replace("#{", "##{");
    format!(
        "$(tmux display-message -p -t '#{{hook_pane}}' {})",
        shell_quote(&context)
    )
}

pub fn http_hook_command(
    base: &str,
    path: &str,
    data: Option<&str>,
    background: bool,
    token_file: &str,
) -> String {
    run_shell_command(&http_hook_script(base, path, data, token_file), background)
}

fn http_hook_script(base: &str, path: &str, data: Option<&str>, token_file: &str) -> String {
    let body = data
        .map(|data| {
            let data = if data == hook_context_format() {
                hook_context_script()
            } else {
                shell_quote(data)
            };
            format!(" --data-binary \"{data}\"")
        })
        .unwrap_or_default();
    format!(
        "token=$(cat {} 2>/dev/null) && curl -s -o /dev/null -m 0.2 --connect-timeout 0.1 -H \"Authorization: Bearer $token\" -X POST {}{body} >/dev/null 2>&1 || true",
        shell_quote(token_file),
        shell_quote(&format!("{base}{path}")),
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run_shell_command(script: &str, background: bool) -> String {
    let background = if background { " -b" } else { "" };
    format!("run-shell{background} \"{}\"", tmux_double_quote(script))
}

fn tmux_double_quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
}

pub fn resized_pane_width_repair_command(base: &str, token_file: &str) -> String {
    run_shell_command(
        &format!(
            "[ '{}' != 1 ] || {}",
            sidebar_width_repair_filter().render(),
            http_hook_script(
                base,
                "/repair-sidebar-width",
                Some(hook_context_format()),
                token_file,
            ),
        ),
        true,
    )
}

pub fn sidebar_mouse_resize_marker_script() -> String {
    format!(
        "tmux -S #{{socket_path}} set-option -gq {SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION} '#{{mouse_window}}'"
    )
}

pub fn sidebar_mouse_resize_report_script(base: &str, token_file: &str) -> String {
    format!(
        "width=$(tmux -S #{{socket_path}} list-panes -t '#{{mouse_window}}' -f '##{{==:##{{pane_title}},{SIDEBAR_PANE_TITLE}}}' -F '##{{pane_width}}' | head -n 1); configured=$(tmux -S #{{socket_path}} show-option -gqv {SIDEBAR_WIDTH_OPTION}); if [ -n \"$width\" ] && [ \"$width\" != \"$configured\" ]; then token=$(cat '{token_file}' 2>/dev/null) && curl -s -o /dev/null -m 0.2 --connect-timeout 0.1 -H \"Authorization: Bearer $token\" -X POST {base}/set-sidebar-width -d \"$width\" >/dev/null 2>&1 || true; fi; tmux -S #{{socket_path}} set-option -gu {SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION} >/dev/null 2>&1 || true"
    )
}

pub fn close_orphan_sidebar_pipeline() -> String {
    format!(
        "tmux -S #{{socket_path}} list-panes -a -f '{}' -F '{}' | while IFS=$(printf '\\t') read -r session pane windows; do if [ \"$windows\" -le 1 ]; then fallback=$(tmux -S #{{socket_path}} list-sessions -F '{}' | awk -v s=\"$session\" '$0==s {{ if (prev != \"\") {{ print prev; exit }}; seen=1; next }} seen {{ print; exit }} {{ prev=$0 }}'); tmux -S #{{socket_path}} list-clients -t \"=$session:\" -F '{}' | while IFS= read -r client; do [ -n \"$client\" ] && [ -n \"$fallback\" ] && tmux -S #{{socket_path}} switch-client -c \"$client\" -t \"=$fallback:\" >/dev/null 2>&1 || true; done; fi; tmux -S #{{socket_path}} kill-pane -t \"$pane\" >/dev/null 2>&1 || true; done",
        orphan_sidebar_filter().render_for_hook(),
        orphan_sidebar_row_format(),
        TmuxFormat::var_name("session_name").render_for_hook(),
        TmuxFormat::var_name("client_tty").render_for_hook(),
    )
}

pub fn pane_exited_hook_command(base: &str, token_file: &str) -> String {
    run_shell_command(
        &format!(
            "{} ; {}",
            close_orphan_sidebar_pipeline(),
            http_hook_script(base, "/pane-exited", None, token_file),
        ),
        true,
    )
}

pub fn pane_died_hook_command(base: &str, token_file: &str) -> String {
    format!(
        "{} ; {}",
        run_shell_command(&close_dead_content_pane_pipeline(), false),
        http_hook_command(base, "/pane-exited", None, false, token_file),
    )
}

pub fn close_dead_content_pane_pipeline() -> String {
    let pane_title = TmuxVar::PaneTitle.format().render_for_hook();
    let pane_dead = TmuxFormat::var_name("pane_dead").render_for_hook();
    let session_id = TmuxFormat::var_name("session_id").render_for_hook();
    let client_tty = TmuxFormat::var_name("client_tty").render_for_hook();

    format!(
        "pane='#{{hook_pane}}'; set -- $(tmux display-message -p -t \"$pane\" '##{{window_id}} ##{{session_id}}'); window=$1; session=$2; counts=$(tmux list-panes -t \"$window\" -F '{pane_title}\t{pane_dead}' | awk -F '\\t' '{{ if ($1==\"opensessions-sidebar\") sidebars++; else if ($2!=\"1\") live++ }} END {{ print sidebars+0, live+0 }}'); set -- $counts; if [ \"$1\" -gt 0 ]; then if [ \"$2\" -eq 0 ]; then windows=$(tmux list-windows -t \"$session\" -F x | wc -l | tr -d ' '); if [ \"$windows\" -le 1 ]; then fallback=$(tmux list-sessions -F '{session_id}' | awk -v s=\"$session\" '$0 != s {{ print; exit }}'); tmux list-clients -t \"$session\" -F '{client_tty}' | while IFS= read -r client; do [ -n \"$client\" ] && [ -n \"$fallback\" ] && tmux switch-client -c \"$client\" -t \"$fallback\" >/dev/null 2>&1 || true; done; fi; tmux kill-window -t \"$window\" >/dev/null 2>&1 || true; else tmux kill-pane -t \"$pane\" >/dev/null 2>&1 || true; fi; fi"
    )
}

fn orphan_sidebar_row_format() -> String {
    [
        TmuxFormat::var_name("session_name"),
        TmuxVar::PaneId.format(),
        TmuxFormat::var_name("session_windows"),
    ]
    .into_iter()
    .map(|format| format.render_for_hook())
    .collect::<Vec<_>>()
    .join("\t")
}

fn sidebar_width_repair_filter() -> TmuxFormat {
    TmuxFormat::and([
        TmuxFormat::gt(TmuxVar::WindowPanes.format(), TmuxFormat::literal("1")),
        TmuxFormat::and([
            sidebar_pane_filter(),
            TmuxFormat::and([
                TmuxFormat::neq(
                    TmuxVar::PaneWidth.format(),
                    TmuxVar::SidebarWidthOption.format(),
                ),
                TmuxFormat::neq(
                    TmuxFormat::var_name("window_id"),
                    TmuxFormat::var_name(SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION),
                ),
            ]),
        ]),
    ])
}

fn orphan_sidebar_filter() -> TmuxFormat {
    TmuxFormat::and([
        TmuxFormat::eq(TmuxVar::WindowPanes.format(), TmuxFormat::literal("1")),
        sidebar_pane_filter(),
    ])
}

fn sidebar_pane_filter() -> TmuxFormat {
    TmuxFormat::eq(
        TmuxVar::PaneTitle.format(),
        TmuxFormat::literal(SIDEBAR_PANE_TITLE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_tmux_formats_once_or_escaped_for_hooks() {
        let filter = TmuxFormat::and([
            TmuxFormat::gt(TmuxVar::WindowPanes.format(), TmuxFormat::literal("1")),
            TmuxFormat::eq(
                TmuxVar::PaneTitle.format(),
                TmuxFormat::literal("opensessions-sidebar"),
            ),
        ]);

        assert_eq!(
            filter.render(),
            "#{&&:#{>:#{window_panes},1},#{==:#{pane_title},opensessions-sidebar}}"
        );
        assert_eq!(
            filter.render_for_hook(),
            "##{&&:##{>:##{window_panes},1},##{==:##{pane_title},opensessions-sidebar}}"
        );
    }

    #[test]
    fn renders_resized_pane_repair_without_a_global_scan() {
        let command = resized_pane_width_repair_command("http://127.0.0.1:45123", "/tmp/token");

        assert!(command.contains("#{==:#{pane_title},opensessions-sidebar}"));
        assert!(command.contains("/repair-sidebar-width"));
        assert!(command.contains("#{pane_id}"));
        assert!(!command.contains("list-panes"));
    }

    #[test]
    fn renders_mouse_resize_as_explicit_width_intent() {
        assert_eq!(
            sidebar_mouse_resize_marker_script(),
            "tmux -S #{socket_path} set-option -gq @opensessions_mouse_resize_window '#{mouse_window}'"
        );
        let report =
            sidebar_mouse_resize_report_script("http://127.0.0.1:1234", "/tmp/opensessions.token");
        assert!(report.contains("list-panes -t '#{mouse_window}'"));
        assert!(report.contains("-f '##{==:##{pane_title},opensessions-sidebar}'"));
        assert!(report.contains("-F '##{pane_width}'"));
        assert!(report.contains("-X POST http://127.0.0.1:1234/set-sidebar-width -d \"$width\""));
        assert!(
            report.ends_with(
                "set-option -gu @opensessions_mouse_resize_window >/dev/null 2>&1 || true"
            )
        );
    }

    #[test]
    fn renders_pane_exited_hook_with_orphan_close_before_server_cleanup() {
        let hook = pane_exited_hook_command("http://127.0.0.1:1234", "/tmp/token");

        assert!(hook.starts_with(
            "run-shell -b \"tmux -S #{socket_path} list-panes -a -f '##{&&:##{==:##{window_panes},1},##{==:##{pane_title},opensessions-sidebar}}'"
        ));
        assert!(
            hook.contains("switch-client -c \\\"\\$client\\\" -t \\\"=\\$fallback:\\\"")
                || hook.contains("switch-client -c \"\\$client\" -t \"=\\$fallback:\"")
        );
        assert!(
            hook.contains("kill-pane -t \\\"\\$pane\\\"")
                || hook.contains("kill-pane -t \"\\$pane\"")
        );
        assert!(hook.contains("-X POST 'http://127.0.0.1:1234/pane-exited'"));
        assert!(!hook.contains("list-panes -a -f '##{&&:##{>:"));
        assert_eq!(hook.matches("run-shell").count(), 1);
    }
}
