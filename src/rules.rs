//! A route's validation rules, read from its code through codemap: the FormRequest the controller method takes
//! (its `rules()`), or a `$request->validate([...])` / `Validator::make(..., [...])` in the method itself. They give
//! the tool a body that passes (the positive case) and bodies that must not (a required field left out).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::plan::CodeLookup;

#[derive(Debug, Clone, Default)]
pub struct Field {
    pub name: String,
    pub rules: Vec<String>,
}

impl Field {
    pub fn has(&self, rule: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r == rule || r.starts_with(&format!("{rule}:")))
    }

    pub fn arg(&self, rule: &str) -> Option<&str> {
        self.rules
            .iter()
            .find_map(|r| r.strip_prefix(&format!("{rule}:")))
    }

    pub fn required(&self) -> bool {
        self.has("required")
    }
}

/// Split the inside of a PHP array (or argument list) at its top-level commas.
fn top_level(text: &str) -> Vec<String> {
    let mut parts = vec![];
    let (mut depth, mut quote, mut current) = (0i32, None::<char>, String::new());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            current.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    current.push(n);
                }
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                current.push(c);
            }
            '[' | '(' | '{' => {
                depth += 1;
                current.push(c);
            }
            ']' | ')' | '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The text inside the first balanced [...] starting at or after `from`.
fn bracket(text: &str, from: usize) -> Option<String> {
    let start = from + text[from..].find('[')?;
    let (mut depth, mut quote) = (0i32, None::<char>);
    for (i, c) in text[start..].char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start + 1..start + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    let q = s.chars().next()?;
    (matches!(q, '\'' | '"') && s.ends_with(q) && s.len() >= 2)
        .then(|| s[1..s.len() - 1].to_string())
}

/// One rule written as code: Rule::in([...]), Rule::exists('t', 'c'), Rule::unique(...), 'required' ...
fn rule_of(item: &str) -> Option<String> {
    static IN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)Rule::in\(\s*\[(.*)\]\s*\)").unwrap());
    static EXISTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"Rule::(exists|unique)\(\s*['"]([\w.]+)['"]\s*(?:,\s*['"](\w+)['"])?"#)
            .unwrap()
    });
    if let Some(s) = unquote(item) {
        return Some(s);
    }
    if let Some(c) = IN.captures(item) {
        let values: Vec<String> = top_level(&c[1])
            .iter()
            .filter_map(|v| unquote(v).or(Some(v.clone())))
            .collect();
        return Some(format!("in:{}", values.join(",")));
    }
    if let Some(c) = EXISTS.captures(item) {
        let table = c[2].rsplit('.').next().unwrap_or(&c[2]).to_string();
        return Some(match c.get(3) {
            Some(col) => format!("{}:{table},{}", &c[1], col.as_str()),
            None => format!("{}:{table}", &c[1]),
        });
    }
    item.contains("Password::").then(|| "password".into())
}

/// Fields of a rules array literal (keys with a dot, such as items.*.id, are left out).
pub fn parse_array(inner: &str) -> Vec<Field> {
    let mut fields = vec![];
    for entry in top_level(inner) {
        let Some((key, value)) = entry.split_once("=>") else {
            continue;
        };
        let Some(name) = unquote(key) else { continue };
        if name.contains('.') || name.contains('*') {
            continue;
        }
        let value = value.trim();
        let rules: Vec<String> = if let Some(s) = unquote(value) {
            s.split('|')
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
                .collect()
        } else if value.starts_with('[') {
            bracket(value, 0)
                .map(|inside| {
                    top_level(&inside)
                        .iter()
                        .filter_map(|i| rule_of(i))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            rule_of(value).into_iter().collect()
        };
        fields.push(Field { name, rules });
    }
    fields
}

/// The source lines of a symbol codemap located.
fn body(lookup: &mut CodeLookup, symbol: &Value) -> Option<(String, String)> {
    let file = lookup.repo().join(symbol["file"].as_str()?);
    let source = std::fs::read_to_string(file).ok()?;
    let start = symbol["line"].as_u64()? as usize;
    let end = symbol["end_line"].as_u64().unwrap_or(start as u64) as usize;
    let lines: Vec<&str> = source
        .lines()
        .skip(start.saturating_sub(1))
        .take(end + 1 - start)
        .collect();
    Some((lines.join("\n"), source))
}

/// Fields from several places, merged: the first rule set that names a field wins.
fn merge(into: &mut Vec<Field>, more: Vec<Field>) {
    for field in more {
        if !into.iter().any(|f| f.name == field.name) {
            into.push(field);
        }
    }
}

/// The rules for a controller action, or None when none could be read. A method often validates in several places
/// (a scope check for project_id, then the record's own rules in a helper), so every one is read and merged.
pub fn for_action(lookup: &mut CodeLookup, action: Option<&str>) -> Option<Vec<Field>> {
    let (class, method) = action?.split_once('@')?;
    let symbol = lookup.symbol(&format!("{class}::{method}"))?;
    let (method_body, source) = body(lookup, &symbol)?;
    let mut fields = vec![];

    // a FormRequest argument: its rules()
    if let Some(params) = symbol["meta"]["params"].as_array() {
        for param in params {
            let Some(kind) = param["type"].as_str() else {
                continue;
            };
            if !kind.ends_with("Request") || kind.ends_with("Http\\Request") || kind == "Request" {
                continue;
            }
            let full = CodeLookup::class_name(kind, &source);
            if let Some(rules) = lookup.symbol(&format!("{full}::rules"))
                && let Some((text, _)) = body(lookup, &rules)
            {
                // the array the method returns (the last `return [`), else the first array in it
                let at = text
                    .rfind("return [")
                    .or_else(|| text.find('['))
                    .unwrap_or(0);
                if let Some(inner) = bracket(&text, at) {
                    merge(&mut fields, parse_array(&inner));
                }
            }
        }
    }

    // $request->validate([...]) / Validator::make($data, [...]) / $this->getValidatorAction($request, [...])
    merge(&mut fields, inline_rules(&method_body));

    // ... and in helpers of the same controller it calls: $this->getValidator($request), $this->rules()
    static HELPER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$this->(\w+)\(").unwrap());
    let mut helpers: Vec<String> = vec![];
    for c in HELPER.captures_iter(&method_body) {
        if !helpers.contains(&c[1].to_string()) {
            helpers.push(c[1].to_string());
        }
    }
    for helper in helpers.iter().take(8) {
        let Some(symbol) = lookup.symbol(&format!("{class}::{helper}")) else {
            continue;
        };
        let Some((text, _)) = body(lookup, &symbol) else {
            continue;
        };
        let found = inline_rules(&text);
        if !found.is_empty() {
            merge(&mut fields, found);
        } else if helper.to_lowercase().contains("rule")
            && let Some(inner) = text.rfind("return [").and_then(|at| bracket(&text, at))
        {
            // a rules() helper that only returns the array
            merge(&mut fields, parse_array(&inner));
        }
    }
    (!fields.is_empty()).then_some(fields)
}

/// The rules of every `->validate([...])`, `Validator::make($data, [...])` and `$this->...Validat...(..., [...])` in
/// a piece of code, merged.
fn inline_rules(code: &str) -> Vec<Field> {
    static INLINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(->validate\(|Validator::make\(|\$this->\w*[vV]alidat\w*\()").unwrap()
    });
    static VARIABLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\s*(\$\w+)\s*(,|$)").unwrap());
    let mut fields = vec![];
    for found in INLINE.find_iter(code) {
        let after = &code[found.end()..];
        // the call's own arguments only
        let Some(args) = call_arguments(after) else {
            continue;
        };
        let at = if found.as_str().starts_with("->validate") {
            0
        } else {
            // skip the first argument (the data, or the request)
            match args.find(',') {
                Some(i) => i + 1,
                None => continue,
            }
        };
        // the rules passed by name ($rules): the array assigned to it before the call
        if let Some(var) = VARIABLE.captures(&args[at..]) {
            let Ok(assign) = Regex::new(&format!(r"{}\s*=\s*\[", regex::escape(&var[1]))) else {
                continue;
            };
            if let Some(start) = assign.find_iter(&code[..found.start()]).last()
                && let Some(inner) = bracket(code, start.start())
            {
                merge(&mut fields, parse_array(&inner));
            }
            continue;
        }
        if args[at..].trim_start().starts_with('[')
            && let Some(inner) = bracket(&args, at)
        {
            merge(&mut fields, parse_array(&inner));
        }
    }
    fields
}

/// The text between a call's parentheses, given the code right after its `(`.
fn call_arguments(after: &str) -> Option<String> {
    let (mut depth, mut quote) = (1i32, None::<char>);
    for (i, c) in after.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' | '[' => depth += 1,
            ']' => depth -= 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(after[..i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// A value that passes the field's rules. `exists` asks the test data through `pick`.
pub fn valid_value(
    field: &Field,
    clock: &str,
    pick: &mut dyn FnMut(&str, &str) -> Option<String>,
) -> Option<Value> {
    if field.has("file") || field.has("image") || field.has("mimes") || field.has("mimetypes") {
        return None;
    }
    if let Some(values) = field.arg("in") {
        return values
            .split(',')
            .next()
            .map(|v| json!(v.trim().trim_matches('"').trim_matches('\'')));
    }
    if let Some(spec) = field.arg("exists") {
        let mut parts = spec.split(',');
        let table = parts.next()?.rsplit('.').next()?.to_string();
        let column = parts
            .next()
            .map(String::from)
            .unwrap_or_else(|| field.name.clone());
        let value = pick(&table, &column)?;
        return Some(
            if field.has("integer") || value.chars().all(|c| c.is_ascii_digit()) {
                value
                    .parse::<i64>()
                    .map(|n| json!(n))
                    .unwrap_or(json!(value))
            } else {
                json!(value)
            },
        );
    }
    let min = field.arg("min").and_then(|m| m.parse::<i64>().ok());
    let max = field.arg("max").and_then(|m| m.parse::<i64>().ok());
    if field.has("boolean") || field.has("accepted") {
        return Some(json!(true));
    }
    if field.has("integer") || field.has("numeric") || field.has("digits") {
        return Some(json!(min.unwrap_or(1).max(if field.has("gt") {
            2
        } else {
            1
        })));
    }
    if field.has("array") {
        return Some(json!([]));
    }
    if field.has("date") || field.has("date_format") || field.has("after") || field.has("before") {
        let day = &clock[..10.min(clock.len())];
        return Some(match field.arg("date_format") {
            Some(f) if f.contains('H') => json!(clock),
            _ => json!(day),
        });
    }
    if field.has("email") {
        return Some(json!("parity@example.test"));
    }
    if field.has("url") {
        return Some(json!("https://example.test/parity"));
    }
    let mut text = if field.has("password") {
        "Parity-Test-1234!".to_string()
    } else {
        format!("parity {}", field.name)
    };
    if let Some(m) = max {
        text.truncate(m.max(1) as usize);
    }
    if let Some(m) = min {
        while (text.len() as i64) < m {
            text.push('x');
        }
    }
    Some(json!(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_in_a_variable() {
        let code = "$rules = ['name' => 'required|max:5', 'qty' => 'integer'];\n$v = \\Validator::make($request->all(), $rules);";
        let names: Vec<String> = inline_rules(code).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["name", "qty"]);
        assert!(
            inline_rules("$rules = [];\n$v = Validator::make($request->all(), $rules);").is_empty()
        );
    }

    #[test]
    fn every_validation_in_a_method() {
        let code = "$this->validateScope();\nrequest()->validate(['project_id' => 'required']);\n\
                    $v = $this->getValidatorAction(request(), ['status_delete_message' => 'required']);\n\
                    $w = \\Validator::make($request->all(), ['project_id' => 'integer', 'date' => 'required|date']);";
        let names: Vec<String> = inline_rules(code).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["project_id", "status_delete_message", "date"]);
    }

    #[test]
    fn parses_rule_arrays() {
        let fields = parse_array(
            "'name' => 'required|string|max:5', 'unit_id' => ['required', 'integer', Rule::exists('units', 'unit_id')],
             'type' => ['nullable', Rule::in(['a', 'b'])], 'items.*.id' => 'required', 'when' => 'date'",
        );
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["name", "unit_id", "type", "when"]);
        assert_eq!(fields[1].arg("exists"), Some("units,unit_id"));
        assert_eq!(fields[2].arg("in"), Some("a,b"));
        let mut pick = |_: &str, _: &str| Some("7".to_string());
        assert_eq!(
            valid_value(&fields[0], "2026-10-30 09:00:00", &mut pick),
            Some(json!("parit"))
        );
        assert_eq!(
            valid_value(&fields[1], "2026-10-30 09:00:00", &mut pick),
            Some(json!(7))
        );
        assert_eq!(
            valid_value(&fields[3], "2026-10-30 09:00:00", &mut pick),
            Some(json!("2026-10-30"))
        );
    }
}
