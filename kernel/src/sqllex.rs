//! Opaque regions of PostgreSQL SQL with standard_conforming_strings enabled.
//!
//! This only finds lexical boundaries; callers retain their own placeholder
//! dialect, supported-syntax and malformed-input policies. In particular,
//! psycopg printf substitution must still run inside quoted text and comments.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    String,
    Identifier,
    DollarString,
    Comment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opaque {
    pub kind: Kind,
    pub end: usize,
    pub terminated: bool,
}

pub fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$') || b >= 0x80
}

pub fn opaque(bytes: &[u8], start: usize) -> Option<Opaque> {
    let c = *bytes.get(start)?;
    let next = bytes.get(start + 1).copied();
    let after_ident = start > 0 && is_ident_byte(bytes[start - 1]);
    if c == b'-' && next == Some(b'-') {
        let end = bytes[start + 2..]
            .iter()
            .position(|b| matches!(b, b'\n' | b'\r'))
            .map_or(bytes.len(), |i| start + 3 + i);
        return Some(Opaque {
            kind: Kind::Comment,
            end,
            terminated: true,
        });
    }
    if c == b'/' && next == Some(b'*') {
        let mut depth = 1;
        let mut i = start + 2;
        while i + 1 < bytes.len() {
            match &bytes[i..i + 2] {
                b"/*" => {
                    depth += 1;
                    i += 2;
                }
                b"*/" => {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        return Some(Opaque {
                            kind: Kind::Comment,
                            end: i,
                            terminated: true,
                        });
                    }
                }
                _ => i += 1,
            }
        }
        return Some(Opaque {
            kind: Kind::Comment,
            end: bytes.len(),
            terminated: false,
        });
    }
    let escaped = matches!(c, b'e' | b'E') && next == Some(b'\'') && !after_ident;
    if matches!(c, b'\'' | b'"') || escaped {
        let quote = if escaped { b'\'' } else { c };
        let kind = if quote == b'"' {
            Kind::Identifier
        } else {
            Kind::String
        };
        let mut i = start + if escaped { 2 } else { 1 };
        while i < bytes.len() {
            if escaped && bytes[i] == b'\\' {
                i += 2;
            } else if bytes[i] == quote {
                i += 1;
                if bytes.get(i) == Some(&quote) {
                    i += 1;
                } else {
                    return Some(Opaque {
                        kind,
                        end: i,
                        terminated: true,
                    });
                }
            } else {
                i += 1;
            }
        }
        return Some(Opaque {
            kind,
            end: bytes.len(),
            terminated: false,
        });
    }
    if c == b'$' && !after_ident && !next.is_some_and(|b| b.is_ascii_digit()) {
        let mut tag_end = start + 1;
        while bytes
            .get(tag_end)
            .is_some_and(|b| *b != b'$' && is_ident_byte(*b))
        {
            tag_end += 1;
        }
        if bytes.get(tag_end) == Some(&b'$') {
            tag_end += 1;
            let tag = &bytes[start..tag_end];
            let end = bytes[tag_end..]
                .windows(tag.len())
                .position(|w| w == tag)
                .map(|i| tag_end + i + tag.len());
            return Some(Opaque {
                kind: Kind::DollarString,
                end: end.unwrap_or(bytes.len()),
                terminated: end.is_some(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_regions_preserve_nested_and_escaped_boundaries() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_test_writer()
            .try_init();
        for text in [
            "'it''s $1'",
            "\"a\"\"$2\"",
            r"E'it\'s ;'",
            "/* a /* b */ c */",
            "$tag$x;$1$tag$",
            "$ñ$x$ñ$",
            "-- comment\r",
        ] {
            let sql = format!("{text}SELECT");
            let token = opaque(sql.as_bytes(), 0).unwrap();
            tracing::debug!(%sql, ?token, "SQL opaque boundary");
            assert!(token.terminated);
            assert_eq!(token.end, text.len());
        }
        for sql in [
            "'open",
            "\"open",
            "/* outer /* inner */",
            "$tag$open",
            "E'open\\",
        ] {
            let token = opaque(sql.as_bytes(), 0).unwrap();
            assert!(!token.terminated, "{sql}");
            assert_eq!(token.end, sql.len());
        }
        for (sql, at) in [("name$1", 4), ("é$x$", 2), ("$1", 0)] {
            assert!(opaque(sql.as_bytes(), at).is_none(), "{sql}");
        }
    }
}
