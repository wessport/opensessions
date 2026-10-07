#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: String,
    pub dirty: bool,
    pub is_worktree: bool,
    pub changed_files: u32,
    pub insertions: u32,
    pub deletions: u32,
}

impl GitInfo {
    pub fn empty() -> Self {
        Self {
            branch: String::new(),
            dirty: false,
            is_worktree: false,
            changed_files: 0,
            insertions: 0,
            deletions: 0,
        }
    }
}

/// Separates the sections of `git_info_output`. Git ref names and porcelain
/// status lines cannot contain NUL, so no branch or path can forge a boundary
/// (a text marker such as `---` is a legal branch name).
pub const GIT_INFO_SECTION_SEPARATOR: char = '\0';

/// Parses `rev-parse --abbrev-ref HEAD --git-dir`, `status --porcelain`, and
/// `diff --numstat` output joined by `GIT_INFO_SECTION_SEPARATOR`.
pub fn parse_git_info_output(output: &str) -> GitInfo {
    let mut sections = output.split(GIT_INFO_SECTION_SEPARATOR);
    let header = sections.next().unwrap_or_default().trim();
    if header.is_empty() {
        return GitInfo::empty();
    }
    let status = sections.next().unwrap_or_default();
    let numstat = sections.next().unwrap_or_default();
    let mut lines = header.lines();
    let branch = lines.next().unwrap_or_default().trim().to_string();
    let git_dir = lines.next().unwrap_or_default().trim();
    let changed_files = status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count() as u32;
    let (insertions, deletions) = parse_numstat_totals(numstat);

    GitInfo {
        branch,
        dirty: changed_files > 0,
        is_worktree: git_dir.contains("/worktrees/"),
        changed_files,
        insertions,
        deletions,
    }
}

fn parse_numstat_totals(numstat: &str) -> (u32, u32) {
    numstat
        .lines()
        .fold((0, 0), |(insertions, deletions), line| {
            let mut fields = line.split_whitespace();
            let added = fields
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            let removed = fields
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            (insertions + added, deletions + removed)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_containing_dashes_do_not_split_sections() {
        let output = [
            "fix---races\n/repo/.git/worktrees/fix",
            " M src/a.rs\n?? b.rs\n",
            "3\t1\tsrc/a.rs\n",
        ]
        .join(&GIT_INFO_SECTION_SEPARATOR.to_string());

        assert_eq!(
            parse_git_info_output(&output),
            GitInfo {
                branch: "fix---races".to_string(),
                dirty: true,
                is_worktree: true,
                changed_files: 2,
                insertions: 3,
                deletions: 1,
            }
        );
    }

    #[test]
    fn empty_output_is_no_git_info() {
        assert_eq!(parse_git_info_output(""), GitInfo::empty());
    }
}
