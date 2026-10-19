//! Just enough BibTeX for papr2.
//!
//! papr2 *reads* `.bib` files to show titles, authors and abstracts, and
//! *writes* new entries (from Google Scholar). It never rewrites a `.bib`
//! file the user has edited, so the parser can be forgiving and the writer
//! simple.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Entry type, e.g. `article`, `inproceedings`.
    pub kind: String,
    /// Citation key, e.g. `vaswani2017attention`.
    pub key: String,
    /// Field names (lowercase) and values without their outer `{}`/`""`.
    pub fields: Vec<(String, String)>,
}

impl Entry {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Sets a field, replacing an existing value or appending a new field.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        match self.fields.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            Some((_, v)) => *v = value,
            None => self.fields.push((name.to_ascii_lowercase(), value)),
        }
    }

    pub fn to_bibtex(&self) -> String {
        let width = self.fields.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        let mut out = format!("@{}{{{},\n", self.kind, self.key);
        for (name, value) in &self.fields {
            let _ = writeln!(out, "  {name:width$} = {{{value}}},");
        }
        out.push_str("}\n");
        out
    }

    /// Checks that writing this entry produces valid BibTeX.
    pub fn validate(&self) -> Result<(), String> {
        if self.kind.is_empty() || !self.kind.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err("The entry type may only contain letters (e.g. article)".into());
        }
        if !valid_key(&self.key) {
            return Err(
                "The citation key can't be empty or contain spaces, commas, braces, quotes, # or %"
                    .into(),
            );
        }
        for (name, value) in &self.fields {
            check_value(value).map_err(|e| format!("{name}: {e}"))?;
        }
        Ok(())
    }
}

/// Whether `key` is usable as a citation key.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ',' | '{' | '}' | '(' | ')' | '"' | '#' | '%' | '\\' | '~' | '\''))
}

/// Field values must have balanced braces to round-trip through BibTeX.
pub fn check_value(value: &str) -> Result<(), String> {
    let mut depth = 0i32;
    let mut escaped = false;
    for c in value.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return Err("has a “}” without a matching “{”".into());
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("has a “{” without a matching “}”".into());
    }
    if escaped {
        return Err("ends with a backslash".into());
    }
    Ok(())
}

/// Parses every entry in `text`, skipping `@comment`, `@preamble` and
/// `@string`. Stops at the first malformed entry.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut p = Parser {
        chars: text.chars().collect(),
        pos: 0,
    };
    let mut entries = Vec::new();
    while p.skip_to('@') {
        p.pos += 1;
        let kind = p.ident().to_ascii_lowercase();
        p.ws();
        let close = match p.peek() {
            Some('{') => '}',
            Some('(') => ')',
            _ => continue, // stray '@' in free text
        };
        p.pos += 1;
        if matches!(kind.as_str(), "comment" | "preamble" | "string") {
            p.skip_balanced(close);
            continue;
        }
        p.ws();
        let key = p.take_while(|c| c != ',' && c != close && !c.is_whitespace());
        p.ws();
        let mut fields = Vec::new();
        loop {
            p.ws();
            match p.peek() {
                Some(',') => {
                    p.pos += 1;
                    continue;
                }
                Some(c) if c == close => {
                    p.pos += 1;
                    break;
                }
                None => return Err(format!("entry “{key}” isn't closed")),
                _ => {}
            }
            let name = p.ident().to_ascii_lowercase();
            if name.is_empty() {
                return Err(format!("unexpected text in entry “{key}”"));
            }
            p.ws();
            if p.peek() != Some('=') {
                return Err(format!("expected “=” after “{name}” in entry “{key}”"));
            }
            p.pos += 1;
            let value = p.value(close).map_err(|e| format!("{name} in “{key}”: {e}"))?;
            fields.push((name, value));
        }
        entries.push(Entry {
            kind,
            key: key.trim().to_string(),
            fields,
        });
    }
    Ok(entries)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    fn skip_to(&mut self, target: char) -> bool {
        while let Some(c) = self.peek() {
            if c == target {
                return true;
            }
            self.pos += 1;
        }
        false
    }

    fn take_while(&mut self, f: impl Fn(char) -> bool) -> String {
        let start = self.pos;
        while self.peek().is_some_and(&f) {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn ident(&mut self) -> String {
        self.take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
    }

    fn skip_balanced(&mut self, close: char) {
        let mut depth = 0;
        while let Some(c) = self.peek() {
            self.pos += 1;
            match c {
                '{' | '(' => depth += 1,
                '}' | ')' if depth > 0 => depth -= 1,
                c if c == close => return,
                _ => {}
            }
        }
    }

    /// A field value: `{...}`, `"..."` or a bare word, joined with `#`.
    fn value(&mut self, close: char) -> Result<String, String> {
        let mut out = String::new();
        loop {
            self.ws();
            match self.peek() {
                Some('{') => {
                    self.pos += 1;
                    out.push_str(&self.delimited('}')?);
                }
                Some('"') => {
                    self.pos += 1;
                    out.push_str(&self.delimited('"')?);
                }
                Some(_) => {
                    let word = self.take_while(|c| c != ',' && c != close && c != '#' && !c.is_whitespace());
                    if word.is_empty() {
                        return Err("missing value".into());
                    }
                    out.push_str(&word);
                }
                None => return Err("missing value".into()),
            }
            self.ws();
            if self.peek() == Some('#') {
                self.pos += 1;
            } else {
                return Ok(out);
            }
        }
    }

    /// Text up to the matching `end`, keeping nested braces.
    fn delimited(&mut self, end: char) -> Result<String, String> {
        let start = self.pos;
        let mut depth = 0;
        while let Some(c) = self.peek() {
            match c {
                '\\' => self.pos += 1,
                '{' => depth += 1,
                '}' if depth > 0 => depth -= 1,
                c if c == end && depth == 0 => {
                    let value = self.chars[start..self.pos].iter().collect();
                    self.pos += 1;
                    return Ok(value);
                }
                _ => {}
            }
            self.pos += 1;
        }
        Err("value isn't closed".into())
    }
}

/// A BibTeX value as plain text: braces removed, common LaTeX escapes
/// and accents converted, whitespace collapsed.
pub fn plain(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' | '}' => {}
            '~' => out.push(' '),
            '\\' => {
                let Some(&next) = chars.peek() else { break };
                if next.is_alphabetic() {
                    let cmd: String = std::iter::from_fn(|| chars.next_if(|c| c.is_alphabetic())).collect();
                    match cmd.as_str() {
                        "L" => out.push('Ł'),
                        "l" => out.push('ł'),
                        "o" => out.push('ø'),
                        "O" => out.push('Ø'),
                        "ss" => out.push('ß'),
                        "ae" => out.push('æ'),
                        "aa" => out.push('å'),
                        "i" => out.push('i'),
                        // \textit{x}, \emph{x}, accents like \c{c}: keep the argument.
                        _ => {}
                    }
                } else {
                    chars.next();
                    match next {
                        '&' | '%' | '$' | '#' | '_' => out.push(next),
                        // Accents like \'e, \"o, \^a: keep the letter.
                        _ => {}
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Author names from an `author` field, as "First Last".
pub fn authors(value: &str) -> Vec<String> {
    plain(value)
        .split(" and ")
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(|a| match a.split_once(',') {
            Some((last, first)) => format!("{} {}", first.trim(), last.trim()),
            None => a.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHOLAR: &str = "@article{vaswani2017attention,\n  title={Attention is all you need},\n  author={Vaswani, Ashish and Kaiser, {\\L}ukasz},\n  journal={Advances in neural information processing systems},\n  volume={30},\n  year={2017}\n}";

    #[test]
    fn parses_scholar_output() {
        let entries = parse(SCHOLAR).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.kind, "article");
        assert_eq!(e.key, "vaswani2017attention");
        assert_eq!(e.get("Title"), Some("Attention is all you need"));
        assert_eq!(e.get("year"), Some("2017"));
        assert_eq!(authors(e.get("author").unwrap()), vec!["Ashish Vaswani", "Łukasz Kaiser"]);
    }

    #[test]
    fn parses_quotes_numbers_concatenation_and_comments() {
        let text = r#"
            @comment{ignored {nested}}
            @string{nips = "NeurIPS"}
            Free text with an @ sign.
            @InProceedings( key:1,
              title = "A {GPU} paper",
              year = 2020,
              note = "part " # {two},
            )"#;
        let e = &parse(text).unwrap()[0];
        assert_eq!(e.kind, "inproceedings");
        assert_eq!(e.key, "key:1");
        assert_eq!(e.get("title"), Some("A {GPU} paper"));
        assert_eq!(e.get("year"), Some("2020"));
        assert_eq!(e.get("note"), Some("part two"));
        assert_eq!(plain(e.get("title").unwrap()), "A GPU paper");
    }

    #[test]
    fn round_trips_written_entries() {
        let mut e = parse(SCHOLAR).unwrap().remove(0);
        e.set("abstract", "We use {\\em attention}: 50\\% faster.");
        e.set("title", "Attention Is All You Need");
        let again = parse(&e.to_bibtex()).unwrap().remove(0);
        assert_eq!(again, e);
        assert_eq!(plain(again.get("abstract").unwrap()), "We use attention: 50% faster.");
    }

    #[test]
    fn reports_malformed_entries() {
        assert!(parse("@article{x, title = {unclosed}").is_err());
        assert!(parse("@article{x, title {missing equals}}").is_err());
    }

    #[test]
    fn validates_entries_before_writing() {
        let mut e = parse(SCHOLAR).unwrap().remove(0);
        assert!(e.validate().is_ok());
        e.set("title", "Broken {title");
        assert!(e.validate().unwrap_err().contains("title"));
        e.set("title", "Fine");
        e.key = "has space".into();
        assert!(e.validate().is_err());
        assert!(check_value("50\\% and \\{literal").is_ok());
        assert!(check_value("x}").is_err());
    }
}
