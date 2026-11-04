//! Windows command-line tokenizer that also reports where each token sits in the raw string.
//!
//! jobexec forwards everything after its own options *verbatim* to `CreateProcessW`, so the
//! child sees exactly the command line the caller built (including `windowsVerbatimArguments`
//! style quoting for `cmd.exe /c`). To do that we need to know where the child's command line
//! begins inside our own raw command line, which `std::env::args` cannot tell us.
//!
//! Rules follow the Microsoft C runtime (post-2008) as documented in
//! "Parsing C command-line arguments":
//! - argv[0]: if it starts with `"`, it runs to the next `"` (no escapes); otherwise to whitespace.
//! - other args are separated by spaces/tabs outside quotes;
//!   `2n` backslashes + `"` -> `n` backslashes and toggles quoting,
//!   `2n+1` backslashes + `"` -> `n` backslashes and a literal `"`,
//!   inside quotes `""` -> a literal `"` (quoting continues),
//!   backslashes not followed by `"` are literal.

/// One parsed token: its value and its `[start, end)` range in UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub value: Vec<u16>,
    pub start: usize,
    pub end: usize,
}

const SPACE: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;
const QUOTE: u16 = b'"' as u16;
const BACKSLASH: u16 = b'\\' as u16;

fn is_ws(c: u16) -> bool {
    c == SPACE || c == TAB
}

pub fn tokenize(cmd: &[u16]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let len = cmd.len();
    let mut i = 0;

    // argv[0]
    if len > 0 {
        let start = i;
        let mut value = Vec::new();
        if cmd[0] == QUOTE {
            i = 1;
            while i < len && cmd[i] != QUOTE {
                value.push(cmd[i]);
                i += 1;
            }
            if i < len {
                i += 1; // closing quote
            }
            // Anything glued to the closing quote still belongs to argv[0].
            while i < len && !is_ws(cmd[i]) {
                value.push(cmd[i]);
                i += 1;
            }
        } else {
            while i < len && !is_ws(cmd[i]) {
                value.push(cmd[i]);
                i += 1;
            }
        }
        tokens.push(Token {
            value,
            start,
            end: i,
        });
    }

    loop {
        while i < len && is_ws(cmd[i]) {
            i += 1;
        }
        if i >= len {
            break;
        }
        let start = i;
        let mut value = Vec::new();
        let mut in_quotes = false;
        while i < len {
            let c = cmd[i];
            if c == BACKSLASH {
                let mut n = 0;
                while i < len && cmd[i] == BACKSLASH {
                    n += 1;
                    i += 1;
                }
                if i < len && cmd[i] == QUOTE {
                    value.extend(std::iter::repeat(BACKSLASH).take(n / 2));
                    if n % 2 == 1 {
                        value.push(QUOTE);
                        i += 1;
                    }
                    // even: leave the quote for the next iteration to toggle
                } else {
                    value.extend(std::iter::repeat(BACKSLASH).take(n));
                }
            } else if c == QUOTE {
                if in_quotes && i + 1 < len && cmd[i + 1] == QUOTE {
                    value.push(QUOTE);
                    i += 2;
                } else {
                    in_quotes = !in_quotes;
                    i += 1;
                }
            } else if is_ws(c) && !in_quotes {
                break;
            } else {
                value.push(c);
                i += 1;
            }
        }
        tokens.push(Token {
            value,
            start,
            end: i,
        });
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }
    fn vals(s: &str) -> Vec<String> {
        tokenize(&w(s))
            .into_iter()
            .map(|t| String::from_utf16(&t.value).unwrap())
            .collect()
    }

    #[test]
    fn microsoft_doc_examples() {
        // Table from "Parsing C command-line arguments".
        assert_eq!(vals(r#"x "a b c" d e"#), ["x", "a b c", "d", "e"]);
        assert_eq!(vals(r#"x "ab\"c" "\\" d"#), ["x", r#"ab"c"#, r"\", "d"]);
        assert_eq!(vals(r#"x a\\\b d"e f"g h"#), ["x", r"a\\\b", "de fg", "h"]);
        assert_eq!(vals(r#"x a\\\"b c d"#), ["x", r#"a\"b"#, "c", "d"]);
        assert_eq!(vals(r#"x a\\\\"b c" d e"#), ["x", r"a\\b c", "d", "e"]);
        assert_eq!(vals(r#"x a"b"" c d"#), ["x", r#"ab" c d"#]);
    }

    #[test]
    fn argv0_quoted_with_spaces() {
        let t = tokenize(&w(r#""C:\Program Files\x.exe" -- a"#));
        assert_eq!(
            String::from_utf16(&t[0].value).unwrap(),
            r"C:\Program Files\x.exe"
        );
        assert_eq!(String::from_utf16(&t[1].value).unwrap(), "--");
        assert_eq!(t[2].start, 28);
    }

    #[test]
    fn spans_point_at_raw_text() {
        let raw = r#"jobexec --wait=main -- "C:\a b\bash.exe" -c "echo \"hi\"""#;
        let t = tokenize(&w(raw));
        let tail: String = raw.chars().skip(t[3].start).collect();
        assert_eq!(tail, r#""C:\a b\bash.exe" -c "echo \"hi\"""#);
        assert_eq!(String::from_utf16(&t[3].value).unwrap(), r"C:\a b\bash.exe");
    }

    #[test]
    fn empty_and_trailing_ws() {
        assert!(tokenize(&[]).is_empty());
        assert_eq!(vals("x   "), ["x"]);
        assert_eq!(vals(r#"x """#), ["x", ""]);
    }
}
