//! A `SKILL.md`'s front matter: the YAML between its first two `---` lines,
//! of which only the top-level `name` and `description` are read. Values may
//! be plain (also continued on indented lines), quoted, or a `|`/`>` block;
//! anything else in it is ignored, and a file without one has neither.

/// What the front matter says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrontMatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// The front matter of `text`, if it begins with one.
#[must_use]
pub fn parse(text: &str) -> FrontMatter {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return FrontMatter::default();
    }
    let body: Vec<&str> = lines.take_while(|line| line.trim_end() != "---").collect();
    let mut found = FrontMatter::default();
    let mut index = 0;
    while index < body.len() {
        let line = body[index];
        index += 1;
        if line.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let slot = match key.trim() {
            "name" => &mut found.name,
            "description" => &mut found.description,
            _ => continue,
        };
        // The indented lines after it belong to this value.
        let start = index;
        while index < body.len() && (body[index].starts_with([' ', '\t']) || body[index].trim().is_empty()) {
            index += 1;
        }
        let value = scalar(value.trim(), &body[start..index]);
        *slot = Some(value).filter(|v| !v.is_empty());
    }
    found
}

/// A scalar from its first line's text and the indented lines that follow.
fn scalar(first: &str, more: &[&str]) -> String {
    let rest = || more.iter().map(|line| line.trim()).collect::<Vec<_>>();
    match first.chars().next() {
        Some('|') => rest().join("\n").trim().to_owned(),
        Some('>') => fold(&rest()),
        Some(quote @ ('"' | '\'')) => {
            let mut joined = first.to_owned();
            for line in rest() {
                joined.push(' ');
                joined.push_str(line);
            }
            unquote(&joined, quote)
        }
        _ => {
            let mut words = vec![first.split(" #").next().unwrap_or_default().trim()];
            words.extend(rest());
            fold(&words)
        }
    }
}

/// Lines joined by spaces, a blank line kept as a line break.
fn fold(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        if line.is_empty() {
            out.push('\n');
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(line);
        }
    }
    out.trim().to_owned()
}

/// The text inside the quotes: `''` is a quote in single quotes, and `\"`,
/// `\\`, `\n` and `\t` are escapes in double quotes.
fn unquote(text: &str, quote: char) -> String {
    let inner = text.strip_prefix(quote).unwrap_or(text);
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == quote {
            if quote == '\'' && chars.peek() == Some(&'\'') {
                chars.next();
                out.push('\'');
                continue;
            }
            break;
        }
        if c == '\\' && quote == '"' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
            continue;
        }
        out.push(c);
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_name_and_description_in_their_yaml_forms() {
        let plain = "---\nname: lean-workflow\ndescription: Route Lean 4 work. Use when editing Lean files.\n---\n# Lean";
        assert_eq!(
            parse(plain),
            FrontMatter {
                name: Some("lean-workflow".into()),
                description: Some("Route Lean 4 work. Use when editing Lean files.".into())
            }
        );
        let folded = "---\nname: 'x'\ndescription: >\n  First line\n  second line.\nlicense: MIT\n---\n";
        assert_eq!(parse(folded).description.as_deref(), Some("First line second line."));
        assert_eq!(parse(folded).name.as_deref(), Some("x"));
        let continued = "---\ndescription: A plain value\n  that goes on.\nname: \"say \\\"hi\\\"\"\n---\n";
        assert_eq!(parse(continued).description.as_deref(), Some("A plain value that goes on."));
        assert_eq!(parse(continued).name.as_deref(), Some("say \"hi\""));
        let literal = "---\ndescription: |\n  one\n  two\n---\n";
        assert_eq!(parse(literal).description.as_deref(), Some("one\ntwo"));
        // Nested keys and comments are not top-level fields.
        let nested = "---\nmetadata:\n  name: inner\n# name: comment\n---\n";
        assert_eq!(parse(nested), FrontMatter::default());
        assert_eq!(parse("# No front matter\nname: x\n"), FrontMatter::default());
    }
}
