//! The generated tests' place in a test file. A file the tool made itself carries MARKER in its class docblock and
//! is the tool's to rewrite or delete. A test file someone wrote (tests/Unit/Models/UnitTest.php already there) gets
//! the generated tests in a block at the end of its class, between START and END: each run replaces only that block,
//! so everything outside it stays as written. Tests an AI assistant writes go in a block of their own (AI_START,
//! AI_END) that the tool never rewrites.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::layout::Place;
use crate::testgen::MARKER;

pub const START: &str = "// <laravel-parity>";
pub const END: &str = "// </laravel-parity>";
pub const AI_START: &str = "// <laravel-parity-ai>";
pub const AI_END: &str = "// </laravel-parity-ai>";

/// The tool made the whole file (MARKER in it, outside any block).
pub fn owned(text: &str) -> bool {
    strip(text).contains(MARKER)
}

/// The text without the generated and AI blocks: what the people who own the file wrote.
pub fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        // the blank line a block is written after goes with it
        let opens = |end: &'static str, out: &mut String| {
            if out.ends_with("\n\n") {
                out.pop();
            }
            Some(end)
        };
        match inside {
            None if trimmed.starts_with(START) => inside = opens(END, &mut out),
            None if trimmed.starts_with(AI_START) => inside = opens(AI_END, &mut out),
            None => out.push_str(line),
            Some(end) if trimmed.starts_with(end) => inside = None,
            Some(_) => {}
        }
    }
    out
}

/// Names of the methods the file declares outside the generated block: a generated method must not take one.
pub fn declared(text: &str) -> HashSet<String> {
    static FUNCTION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"function\s+&?\s*(\w+)\s*\(").unwrap());
    let outside = text_without_generated(text);
    FUNCTION
        .captures_iter(&outside)
        .map(|c| c[1].to_string())
        .collect()
}

fn text_without_generated(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if !inside && trimmed.starts_with(START) {
            inside = true;
        } else if inside && trimmed.starts_with(END) {
            inside = false;
        } else if !inside {
            out.push_str(line);
        }
    }
    out
}

/// The class a test file declares, with its namespace.
pub fn class_of(text: &str) -> Option<String> {
    static CLASS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(?:final\s+|abstract\s+)*class\s+(\w+)").unwrap());
    static NAMESPACE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*namespace\s+([\w\\]+)\s*;").unwrap());
    let class = CLASS.captures(text)?[1].to_string();
    Some(match NAMESPACE.captures(text) {
        Some(n) => format!("{}\\{class}", &n[1]),
        None => class,
    })
}

/// Whether the generated tests can go in this existing file: one concrete class to add them to.
pub fn usable(text: &str) -> Result<(), String> {
    static ABSTRACT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*abstract\s+class\s").unwrap());
    if class_of(text).is_none() {
        return Err("declares no class".into());
    }
    if ABSTRACT.is_match(text) {
        return Err("is an abstract class".into());
    }
    if !text.trim_end().ends_with('}') {
        return Err("does not end with its class's closing brace".into());
    }
    Ok(())
}

fn block(members: &[String]) -> String {
    let mut out = vec![format!(
        "    {START} generated from the old app's answers: replaced on every run, so edit outside this block"
    )];
    for (i, member) in members.iter().enumerate() {
        if i > 0 {
            out.push(String::new());
        }
        out.extend(member.lines().map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("    {l}")
            }
        }));
    }
    out.push(format!("    {END}"));
    out.join("\n")
}

/// The file's new text with `members` (methods, unindented) as its generated block. `existing` is the file as it
/// is (None: there is none); a file the tool owns, or none, is written whole with `doc` (the class docblock lines,
/// with MARKER) and `base` (the class it extends).
pub fn render(
    existing: Option<&str>,
    place: &Place,
    doc: &[String],
    base: &str,
    members: &[String],
) -> String {
    let block = block(members);
    match existing {
        Some(text) if !owned(text) => {
            let outside = text_without_generated(text);
            let trimmed = outside.trim_end();
            // the class's closing brace is the file's last one
            let body = trimmed.strip_suffix('}').unwrap_or(trimmed).trim_end();
            format!("{body}\n\n{block}\n}}\n")
        }
        _ => {
            let mut out = vec![
                "<?php".to_string(),
                String::new(),
                format!("namespace {};", place.namespace),
                String::new(),
            ];
            out.extend(doc.iter().cloned());
            out.push(format!("class {} extends \\{base}", place.class));
            out.push("{".into());
            out.push(block);
            out.push("}".into());
            out.join("\n") + "\n"
        }
    }
}

/// Take the generated tests out of a file this run did not write: delete a file the tool owns, else remove its
/// block. True when the file changed.
pub fn clear(file: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else {
        return false;
    };
    if owned(&text) {
        return std::fs::remove_file(file).is_ok();
    }
    if !text.lines().any(|l| l.trim().starts_with(START)) {
        return false;
    }
    let outside = text_without_generated(&text);
    // the blank line the block was written after
    let cleaned = outside.trim_end();
    let cleaned = match cleaned.strip_suffix('}') {
        Some(body) => format!("{}\n}}\n", body.trim_end()),
        None => format!("{cleaned}\n"),
    };
    std::fs::write(file, cleaned).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn place() -> Place {
        Place {
            file: PathBuf::from("tests/Unit/Models/UnitTest.php"),
            namespace: "Tests\\Unit\\Models".into(),
            class: "UnitTest".into(),
        }
    }

    const MINE: &str = "<?php\n\nnamespace Tests\\Unit\\Models;\n\nclass UnitTest extends ModelTestCase\n{\n    public function test_owner(): void\n    {\n        $this->assertTrue(true);\n    }\n}\n";

    #[test]
    fn a_block_in_someone_elses_file() {
        let members = vec!["public function test_owner_2(): void\n{\n    $x = 1;\n}".to_string()];
        let once = render(Some(MINE), &place(), &[], "Tests\\TestCase", &members);
        assert!(once.starts_with(MINE.trim_end().strip_suffix('}').unwrap().trim_end()));
        assert!(once.contains("    // <laravel-parity> generated"));
        assert!(
            once.contains(
                "    public function test_owner_2(): void\n    {\n        $x = 1;\n    }\n"
            )
        );
        assert!(once.ends_with("    // </laravel-parity>\n}\n"));
        assert!(!owned(&once));
        assert_eq!(strip(&once).trim_end(), MINE.trim_end());
        assert_eq!(class_of(&once).unwrap(), "Tests\\Unit\\Models\\UnitTest");
        // the methods outside the block are taken; the generated ones are not
        let names = declared(&once);
        assert!(names.contains("test_owner") && !names.contains("test_owner_2"));
        // a second run replaces the block, it does not add another
        let twice = render(
            Some(&once),
            &place(),
            &[],
            "Tests\\TestCase",
            &["public function test_other(): void\n{\n}".to_string()],
        );
        assert_eq!(twice.matches(START).count(), 1);
        assert!(twice.contains("test_other") && !twice.contains("test_owner_2"));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("UnitTest.php");
        std::fs::write(&file, &twice).unwrap();
        assert!(clear(&file));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), MINE);
        assert!(!clear(&file));
    }

    #[test]
    fn a_file_of_its_own() {
        let doc = vec!["/**".to_string(), format!(" * {MARKER}: x"), " */".into()];
        let members = vec!["public function test_a(): void\n{\n}".to_string()];
        let text = render(None, &place(), &doc, "Tests\\TestCase", &members);
        assert!(text.starts_with("<?php\n\nnamespace Tests\\Unit\\Models;\n\n/**\n"));
        assert!(
            text.contains("class UnitTest extends \\Tests\\TestCase\n{\n    // <laravel-parity>")
        );
        assert!(owned(&text));
        // rewritten whole, not appended to
        let again = render(Some(&text), &place(), &doc, "Tests\\TestCase", &members);
        assert_eq!(again, text);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("UnitTest.php");
        std::fs::write(&file, &text).unwrap();
        assert!(clear(&file) && !file.exists());
    }

    #[test]
    fn ai_blocks_stay_and_do_not_count() {
        let text = format!(
            "<?php\nclass A extends B\n{{\n    {AI_START}\n    public function test_ai(): void {{}}\n    {AI_END}\n}}\n"
        );
        assert!(!strip(&text).contains("test_ai"));
        assert!(declared(&text).contains("test_ai"));
        let with = render(
            Some(&text),
            &place(),
            &[],
            "B",
            &["public function test_g(): void {}".into()],
        );
        assert!(with.contains("test_ai") && with.contains("test_g"));
        assert!(usable(&text).is_ok());
        assert!(usable("<?php\nabstract class A {\n}\n").is_err());
        assert!(usable("<?php\n// nothing\n").is_err());
    }
}
