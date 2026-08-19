//! `KEY=VALUE` files, parsed and never executed.
//!
//! The accepted grammar is the intersection of what dotenv tools agree on,
//! minus every construct they differ about. In particular `$` is a character:
//! no expansion, no command substitution. A file that reaches `execve` as
//! `envp` should mean what it says, and "what it says" should not depend on
//! which dotenv implementation you last used.

use std::collections::BTreeMap;

/// A line that did not parse, addressed for `doctor`. The offending text is
/// deliberately absent: these files hold secrets, and a diagnostic that quotes
/// the line defeats the point of keeping values out of the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadLine {
    pub line: usize,
    pub reason: &'static str,
}

/// Parsed keys in file order, plus whatever did not parse.
#[derive(Debug, Clone, Default)]
pub struct EnvFile {
    pub vars: Vec<(String, String)>,
    pub bad: Vec<BadLine>,
}

pub fn parse(text: &str) -> EnvFile {
    let mut out = EnvFile::default();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();
        // `#` opens a comment only at the start of a line, so `hunter2#3` is
        // the password you meant.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let body = trimmed
            .strip_prefix("export ")
            .unwrap_or(trimmed)
            .trim_start();
        let Some((key, value)) = body.split_once('=') else {
            out.bad.push(BadLine {
                line,
                reason: "no '=' in line",
            });
            continue;
        };
        let key = key.trim();
        if !is_identifier(key) {
            out.bad.push(BadLine {
                line,
                reason: "key is not [A-Za-z_][A-Za-z0-9_]*",
            });
            continue;
        }
        match unquote(value.trim()) {
            // Last wins within one file, so `vars` is the file's effective
            // content — otherwise a duplicated key is counted twice and
            // `@muster env` lists it twice.
            Some(value) => {
                out.vars.retain(|(existing, _)| existing != key);
                out.vars.push((key.to_string(), value));
            }
            None => out.bad.push(BadLine {
                line,
                reason: "unterminated quote",
            }),
        }
    }
    out
}

fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `'literal'`, `"escaped"`, or bare-to-end-of-line.
fn unquote(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    match bytes.first() {
        Some(b'\'') => {
            if bytes.len() < 2 || !value.ends_with('\'') {
                return None;
            }
            Some(value[1..value.len() - 1].to_string())
        }
        Some(b'"') => {
            if bytes.len() < 2 || !value.ends_with('"') {
                return None;
            }
            let inner = &value[1..value.len() - 1];
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    out.push(c);
                    continue;
                }
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    // An unknown escape keeps both characters: this is data,
                    // and inventing a meaning for `\d` would be worse than
                    // passing it through.
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            }
            Some(out)
        }
        _ => Some(value.to_string()),
    }
}

/// Merge files in order, then explicit `env`, into what `CREATE2` will carry.
///
/// Later wins at every level: a later file beats an earlier one, and `env`
/// beats every file. Everything the server derives sits underneath, since the
/// environment block is applied last on top of it.
pub fn merge(
    files: &[(String, EnvFile)],
    env: &BTreeMap<String, String>,
) -> Vec<(String, String, Origin)> {
    let mut merged: BTreeMap<String, (String, Origin)> = BTreeMap::new();
    for (path, file) in files {
        for (key, value) in &file.vars {
            merged.insert(key.clone(), (value.clone(), Origin::File(path.clone())));
        }
    }
    for (key, value) in env {
        merged.insert(key.clone(), (value.clone(), Origin::Unit));
    }
    merged
        .into_iter()
        .map(|(k, (v, origin))| (k, v, origin))
        .collect()
}

/// Which source won a key, so `@muster env` can answer "which of my three
/// `.env` files won" without printing values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    File(String),
    Unit,
}

impl Origin {
    pub fn label(&self) -> &str {
        match self {
            Origin::File(path) => path,
            Origin::Unit => "(env)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(text: &str) -> Vec<(String, String)> {
        parse(text).vars
    }

    #[test]
    fn a_hash_inside_a_value_is_not_a_comment() {
        assert_eq!(
            vars("PASSWORD=hunter2#3"),
            vec![("PASSWORD".into(), "hunter2#3".into())]
        );
        assert!(vars("# PASSWORD=x").is_empty());
        assert!(vars("   # indented comment").is_empty());
    }

    #[test]
    fn export_is_stripped_and_whitespace_trimmed() {
        assert_eq!(
            vars("export PORT=8080"),
            vec![("PORT".into(), "8080".into())]
        );
        assert_eq!(vars("  A = b  "), vec![("A".into(), "b".into())]);
    }

    #[test]
    fn unquoted_values_keep_their_spaces() {
        assert_eq!(
            vars("GREETING=hello world"),
            vec![("GREETING".into(), "hello world".into())]
        );
    }

    #[test]
    fn single_quotes_are_literal_and_double_quotes_escape() {
        assert_eq!(
            vars(r#"A='no $expansion, no \n escapes'"#),
            vec![("A".into(), r"no $expansion, no \n escapes".into())]
        );
        assert_eq!(
            vars(r#"B="tab\there""#),
            vec![("B".into(), "tab\there".into())]
        );
    }

    #[test]
    fn dollars_are_never_expanded() {
        assert_eq!(vars("A=$HOME"), vec![("A".into(), "$HOME".into())]);
        assert_eq!(vars("B=$(whoami)"), vec![("B".into(), "$(whoami)".into())]);
    }

    #[test]
    fn bad_lines_are_located_but_not_quoted() {
        let parsed = parse("GOOD=1\nnonsense\n9BAD=1\nC='unterminated\n");
        assert_eq!(parsed.vars.len(), 1);
        assert_eq!(parsed.bad.len(), 3);
        assert_eq!(parsed.bad[0].line, 2);
        assert_eq!(parsed.bad[1].line, 3);
        assert_eq!(parsed.bad[2].line, 4);
        // Nothing in a diagnostic may carry the value.
        for bad in &parsed.bad {
            assert!(!bad.reason.contains("unterminated\n"));
        }
    }

    #[test]
    fn last_wins_within_a_file() {
        assert_eq!(vars("A=1\nA=2"), vec![("A".into(), "2".into())]);
    }

    #[test]
    fn later_files_win_and_env_beats_all_of_them() {
        let first = ("a.env".to_string(), parse("A=1\nB=1\nC=1"));
        let second = ("b.env".to_string(), parse("B=2\nC=2"));
        let mut env = BTreeMap::new();
        env.insert("C".to_string(), "3".to_string());
        let merged = merge(&[first, second], &env);
        let by_key: BTreeMap<_, _> = merged
            .iter()
            .map(|(k, v, o)| (k.as_str(), (v.as_str(), o.label())))
            .collect();
        assert_eq!(by_key["A"], ("1", "a.env"));
        assert_eq!(by_key["B"], ("2", "b.env"));
        assert_eq!(by_key["C"], ("3", "(env)"));
    }
}
