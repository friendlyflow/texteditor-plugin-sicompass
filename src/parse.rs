use sicompass_sdk::ffon::FfonElement;
use sicompass_sdk::tags;

/// Turn a text file into its rows: one per source line, verbatim, the way a
/// text editor shows it. Indentation, tabs and trailing spaces are kept, and a
/// blank line is an empty row. Braces and `:` open no levels (only a `.ff`
/// file has levels, see `ff.rs`).
///
/// Every row is annotated with `<src=N>`, N being the 0-based source-line
/// index. The annotation is invisible in the UI (stripped by `strip_display`)
/// but lets `commit_edit` map each row back to the line it changes on disk.
pub fn parse_lines(contents: &str) -> Vec<FfonElement> {
    contents
        .lines()
        .enumerate()
        .map(|(n, line)| FfonElement::new_str(format!("{}{}", tags::format_src(n), line)))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_src(s: &str) -> &str {
        tags::extract_src(s).map(|(_, rest)| rest).unwrap_or(s)
    }

    fn texts(elements: &[FfonElement]) -> Vec<&str> {
        elements
            .iter()
            .map(|e| strip_src(e.as_str().expect("every row is a Str")))
            .collect()
    }

    #[test]
    fn splits_on_newline() {
        let elements = parse_lines("line1\nline2\nline3");
        assert_eq!(texts(&elements), ["line1", "line2", "line3"]);
    }

    #[test]
    fn blank_line_is_an_empty_row() {
        let elements = parse_lines("a\n\nb");
        assert_eq!(texts(&elements), ["a", "", "b"]);
    }

    #[test]
    fn keeps_leading_spaces() {
        let elements = parse_lines("void foo() {\n    return;\n}");
        assert_eq!(texts(&elements), ["void foo() {", "    return;", "}"]);
    }

    #[test]
    fn keeps_tabs() {
        let elements = parse_lines("all:\n\tcc -o x x.c\n\t\techo nested");
        assert_eq!(texts(&elements), ["all:", "\tcc -o x x.c", "\t\techo nested"]);
    }

    #[test]
    fn keeps_trailing_spaces() {
        let elements = parse_lines("x = 1  \n  ");
        assert_eq!(texts(&elements), ["x = 1  ", "  "]);
    }

    #[test]
    fn braces_open_no_levels() {
        let elements = parse_lines("fn outer() {\n    fn inner() {\n        x\n    }\n}");
        assert_eq!(elements.len(), 5);
        assert!(elements.iter().all(FfonElement::is_str));
        assert_eq!(strip_src(elements[2].as_str().unwrap()), "        x");
    }

    #[test]
    fn python_colon_opens_no_level() {
        let elements = parse_lines("class Foo:\n    def bar(self):\n        pass\n");
        assert_eq!(
            texts(&elements),
            ["class Foo:", "    def bar(self):", "        pass"]
        );
    }

    #[test]
    fn ffon_section_braces_are_plain_lines() {
        let elements = parse_lines("section:\n{\n  child\n}");
        assert_eq!(texts(&elements), ["section:", "{", "  child", "}"]);
    }

    #[test]
    fn src_annotations_match_line_indices() {
        let elements = parse_lines("alpha\n\n  beta\ngamma");
        let srcs: Vec<_> = elements
            .iter()
            .map(|e| tags::extract_src(e.as_str().unwrap()).map(|(n, _)| n))
            .collect();
        assert_eq!(srcs, [Some(0), Some(1), Some(2), Some(3)]);
    }
}
