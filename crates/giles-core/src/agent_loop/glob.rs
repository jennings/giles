//! Tool-name globs: an agent's `tools` allowlist and its `ask` list (PLAN section 6.4).
//!
//! `*` matches any run of characters (including none) and `?` exactly one; every other character
//! matches itself, case-sensitively. A glob matches the whole name: `memory*` matches `memory_add`
//! and `memory_search`, `github__*` every tool of the `github` toolset.

/// Whether `glob` matches the whole of `name`.
pub fn matches(glob: &str, name: &str) -> bool {
    let glob: Vec<char> = glob.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut g, mut n) = (0, 0);
    // The position after the last `*` seen, and the name position it was tried against.
    let mut backtrack: Option<(usize, usize)> = None;
    while n < name.len() {
        match glob.get(g) {
            Some('*') => {
                g += 1;
                backtrack = Some((g, n));
            }
            Some(&c) if c == '?' || c == name[n] => {
                g += 1;
                n += 1;
            }
            _ => match backtrack {
                // Let the last `*` swallow one more character and retry from there.
                Some((after_star, tried)) => {
                    g = after_star;
                    n = tried + 1;
                    backtrack = Some((after_star, tried + 1));
                }
                None => return false,
            },
        }
    }
    glob[g..].iter().all(|&c| c == '*')
}

/// Whether any of `globs` matches `name`.
pub fn matches_any<S: AsRef<str>>(globs: &[S], name: &str) -> bool {
    globs.iter().any(|glob| matches(glob.as_ref(), name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_match_only_the_whole_name() {
        assert!(matches("memory_add", "memory_add"));
        assert!(!matches("memory_add", "memory_add_x"));
        assert!(!matches("memory_search", "memory_add"));
        assert!(!matches("Memory_add", "memory_add"));
        assert!(!matches("", "memory_add"));
        assert!(matches("", ""));
    }

    #[test]
    fn star_matches_any_run_including_none() {
        assert!(matches("memory_add*", "memory_add"));
        assert!(matches("memory*", "memory_add"));
        assert!(matches("memory*", "memory_search"));
        assert!(matches("github__*", "github__create_issue"));
        assert!(!matches("github__*", "gitlab__create_issue"));
        assert!(!matches("github__*", "github_x"));
        assert!(matches("*", ""));
        assert!(matches("*", "anything"));
        assert!(matches("*__read*", "fs__read_file"));
        assert!(matches("*_issue", "github__create_issue"));
        assert!(!matches("*_issue", "github__create_issues"));
        // Backtracking past an early partial match.
        assert!(matches("*ab*abc", "xabyabababc"));
        assert!(!matches("*ab*abc", "xabyababab"));
        assert!(matches("a**b", "ab"));
    }

    #[test]
    fn question_mark_matches_exactly_one_character() {
        assert!(matches("fs__rea?", "fs__read"));
        assert!(!matches("fs__rea?", "fs__rea"));
        assert!(!matches("fs__rea?", "fs__reads"));
        assert!(matches("?é", "çé"));
    }

    #[test]
    fn any_of_several_globs() {
        let globs = ["github__*", "memory*"];
        assert!(matches_any(&globs, "memory_search"));
        assert!(matches_any(&globs, "github__list"));
        assert!(!matches_any(&globs, "load_skill"));
        assert!(!matches_any::<&str>(&[], "memory_add"));
    }
}
