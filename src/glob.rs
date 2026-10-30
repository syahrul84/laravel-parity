//! Shell-style matching with `*` and `?` only. Brackets are ordinary characters, so a JSON path pattern such as
//! `$.meta.links[*].page` matches `$.meta.links[3].page`.

pub fn matches(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn any(text: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| matches(text, p))
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn stars_and_brackets() {
        assert!(matches("api/admin/units", "api/*"));
        assert!(matches("GET api/admin/x/import", "* api/*/import*"));
        assert!(matches("$.meta.links[3].page", "$.meta.links[*].page"));
        assert!(matches("unit_project_id", "*project_id"));
        assert!(!matches("project", "*project_id"));
        assert!(matches("a", "?"));
        assert!(!matches("ab", "?"));
    }
}
