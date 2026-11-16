//! Shell words, as the prompt's text holds them: where they are (quotes and
//! backslashes taken into account), and quoting and unquoting them. Shared
//! by tab completion and `sel`, which must agree on what the word at the
//! cursor is.

/// The [start, end) spans (in characters) of the space-separated units of a
/// command line, as the shell splits them: spaces in quotes or after a
/// backslash don't separate. (An unclosed quote runs to the end.)
pub fn units(text: &[char]) -> Vec<(usize, usize)> {
    let n = text.len();
    let find = |c: char, from: usize| (from..n).find(|&j| text[j] == c);
    let mut spans = vec![];
    let mut i = 0;
    while i < n {
        if text[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && !text[i].is_whitespace() {
            let c = text[i];
            if c == '\\' {
                i += 2;
            } else if c == '\'' || c == '"' {
                let mut close = find(c, i + 1);
                while c == '"'
                    && let Some(k) = close
                    && text[k - 1] == '\\'
                {
                    close = find(c, k + 1);
                }
                i = close.map_or(n, |k| k + 1);
            } else {
                i += 1;
            }
        }
        spans.push((start, i.min(n)));
    }
    spans
}

/// The word being typed: the unit (see units) that ends at byte `position`
/// of `text`, as typed so far, and where it starts (`position` itself, and
/// an empty word, after a space).
pub fn word_before(text: &str, position: usize) -> (usize, &str) {
    let left = &text[..position];
    let chars: Vec<char> = left.chars().collect();
    let start = match units(&chars).last() {
        Some(&(s, e)) if e == chars.len() => left.char_indices().nth(s).map_or(position, |(i, _)| i),
        _ => position,
    };
    (start, &left[start..])
}

/// Shell words, as Python's `shlex.split` makes them (the `shlex` crate's,
/// but with `#` an ordinary character, not a comment). None: unbalanced
/// quotes.
pub fn split_words(text: &str) -> Option<Vec<String>> {
    const HASH: char = '\u{E000}';
    let words = shlex::split(&text.replace('#', &HASH.to_string()))?;
    Some(words.into_iter().map(|w| w.replace(HASH, "#")).collect())
}

/// `text` as the shell would read it, if it is one word ('a b' -> a b); as
/// it is otherwise.
pub fn unquote(text: &str) -> String {
    match split_words(text) {
        Some(words) if words.len() == 1 => words.into_iter().next().unwrap(),
        _ => text.to_owned(),
    }
}

/// A word being typed, as the shell will read it (its quote closed, if it
/// is still open): None if it isn't one word.
pub fn unquote_partial(word: &str) -> Option<String> {
    ["", "'", "\""]
        .iter()
        .find_map(|close| match split_words(&format!("{word}{close}")) {
            Some(words) if words.len() == 1 => words.into_iter().next(),
            _ => None,
        })
}

/// A word quoted for the shell, as Python's `shlex.quote` does it: as is if
/// it is safe, else in single quotes.
pub fn quote(word: &str) -> String {
    if word.is_empty() {
        return "''".into();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if word.chars().all(safe) {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn splits_units() {
        assert_eq!(
            units(&chars(r#"ls 'a b' "c\" d" e\ f"#)),
            vec![(0, 2), (3, 8), (9, 16), (17, 21)]
        );
    }

    #[test]
    fn words_before_the_cursor() {
        assert_eq!(word_before("ls  fo", 6), (4, "fo"));
        assert_eq!(word_before("ls ", 3), (3, ""));
        assert_eq!(word_before("cat 'my fi", 10), (4, "'my fi"));
        assert_eq!(word_before(r"cat a\ b", 8), (4, r"a\ b"));
        assert_eq!(word_before("é 'à b", 8), (3, "'à b"));
    }

    #[test]
    fn quotes_like_python() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a/b-c.d=e%f"), "a/b-c.d=e%f");
        assert_eq!(quote("it's"), r#"'it'"'"'s'"#);
        assert_eq!(quote("a b"), "'a b'");
    }

    #[test]
    fn unquotes_one_word() {
        assert_eq!(unquote("'a b'"), "a b");
        assert_eq!(unquote("#x"), "#x");
        assert_eq!(unquote("a b"), "a b");
        assert_eq!(unquote("'open"), "'open");
        assert_eq!(unquote_partial("'open sesame").as_deref(), Some("open sesame"));
        assert_eq!(unquote_partial(r#""a\"b"#).as_deref(), Some("a\"b"));
        assert_eq!(unquote_partial(r"a\ b").as_deref(), Some("a b"));
        assert_eq!(unquote_partial("a b"), None);
    }
}
