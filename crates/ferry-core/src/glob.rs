//! Minimal `*`/`?` wildcard matching as used by `ssh_config` and known_hosts.

/// Whether `text` matches `pattern`, where `*` matches any run of characters
/// (including none) and `?` matches exactly one character.
///
/// Runs in `O(pattern × text)` worst case without recursion, so hostile input
/// cannot blow the stack.
pub(crate) fn matches(pattern: &str, text: &str, ignore_ascii_case: bool) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let eq = |a: char, b: char| {
        if ignore_ascii_case {
            a.eq_ignore_ascii_case(&b)
        } else {
            a == b
        }
    };

    let (mut p, mut t) = (0, 0);
    // Position after the most recent `*` and the text position it was tried at.
    let mut backtrack: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                p += 1;
                backtrack = Some((p, t));
            }
            Some(&c) if c == '?' || eq(c, text[t]) => {
                p += 1;
                t += 1;
            }
            _ => match backtrack {
                Some((star_p, star_t)) => {
                    p = star_p;
                    t = star_t + 1;
                    backtrack = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn wildcards() {
        assert!(matches("*", "", false));
        assert!(matches("*", "anything", false));
        assert!(matches("*.example.org", "a.b.example.org", false));
        assert!(!matches("*.example.org", "example.org", false));
        assert!(matches("host?", "host1", false));
        assert!(!matches("host?", "host", false));
        assert!(matches("a*b*c", "aXXbYYc", false));
        assert!(!matches("a*b*c", "aXXbYY", false));
        assert!(matches("[h*]:22?2", "[host]:2222", false));
        assert!(matches("10.0.0.*", "10.0.0.17", false));
    }

    #[test]
    fn case_handling() {
        assert!(matches("HOST", "host", true));
        assert!(!matches("HOST", "host", false));
    }

    #[test]
    fn pathological_input_is_fast() {
        let text = "a".repeat(10_000);
        assert!(!matches(&format!("{}b", "*a".repeat(50)), &text, false));
    }
}
