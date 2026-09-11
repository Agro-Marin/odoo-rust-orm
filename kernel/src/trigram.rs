//! Odoo's translated-trigram accelerator, ported from `odoo/libs/sql/trigram.py`.
//!
//! A translated field declared `index="trigram"` carries a GIN index on
//! **one specific expression**:
//!
//! ```sql
//! gin (unaccent(jsonb_path_query_array(name, '$.*')::text) gin_trgm_ops)
//! ```
//!
//! Nothing but that expression can use it, and Odoo's `_String.condition_to_sql`
//! ANDs exactly it onto every `like` / `ilike` / single-valued `in`. The
//! conjunct is implied by the condition it accompanies -- the array of every
//! translation contains the one the base condition tests -- so it changes no
//! row and buys the index. A kernel that omits it answers the same rows by
//! sequential scan, which on a large `product.template` is slower than the
//! Python it replaced.
//!
//! The two functions are ports rather than reimplementations, and the tests
//! below are Odoo's own output on the same inputs: 40,040 strings over an
//! alphabet of wildcards, backslashes, quotes, tabs, newlines and non-ASCII
//! agreed exactly, which is what licenses the hand-written scanner in place of
//! `_TRIGRAM_PATTERN_RE` (whose lookbehind Rust's `regex` cannot express).

/// `json.dumps(s, ensure_ascii=False)[1:-1]` -- RFC 8259 string escaping with
/// the surrounding quotes removed, which leaves non-ASCII alone.
fn json_escape(s: &str) -> String {
    let quoted = serde_json::Value::String(s.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

/// Escape what PostgreSQL's LIKE treats as special, so the pattern matches the
/// characters literally.
fn escape_wildcards(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(c, '_' | '%' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The accelerator pattern for an `in` against a single value.
pub fn value_to_pattern(value: &str) -> String {
    if value.chars().count() < 3 {
        return "%".into();
    }
    format!("%{}%", escape_wildcards(&json_escape(value)))
}

/// The accelerator pattern for a `like` / `ilike` pattern.
///
/// Splits on the wildcards the caller did NOT escape, unescaping as it goes,
/// and keeps the runs of three characters or more -- a shorter run cannot
/// constrain a trigram index, so it is dropped rather than narrowing the
/// conjunct below what the base condition guarantees.
pub fn pattern_to_pattern(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let n = chars.len();
    let mut segments: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut dangling = false;
    let mut i = 0;
    while i < n {
        match chars[i] {
            '\\' if i + 1 < n => {
                cur.push(chars[i + 1]);
                i += 2;
            }
            // A backslash with nothing to escape is a pattern Python's regex
            // declines to match at all, which drops the segment it ends --
            // and only that one; the segments before it still count.
            '\\' => {
                dangling = true;
                i += 1;
            }
            '_' | '%' => {
                segments.push(std::mem::take(&mut cur));
                i += 1;
            }
            // Python's `$` matches before a single trailing newline, so a
            // segment ends there -- but only when the scan REACHES it
            // unescaped. `\<newline>` consumes it as a literal and the match
            // ends at the true end instead, which is why this is a scanner
            // state and not a `strip_suffix`.
            '\n' if i == n - 1 => {
                segments.push(std::mem::take(&mut cur));
                i += 1;
            }
            c => {
                cur.push(c);
                i += 1;
            }
        }
    }
    if !dangling {
        segments.push(cur);
    }
    let kept: Vec<String> = segments
        .iter()
        .filter(|t| t.chars().count() >= 3)
        .map(|t| escape_wildcards(&json_escape(t)))
        .collect();
    if kept.is_empty() {
        "%".into()
    } else {
        format!("%{}%", kept.join("%"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // (input, value_to_pattern, pattern_to_pattern), taken from Odoo 19's own
    // `odoo.libs.sql.trigram` rather than derived from the code above.
    const PAIRS: &[(&str, &str, &str)] = &[
        ("", "%", "%"),
        ("a", "%", "%"),
        ("ab", "%", "%"),
        ("abc", "%abc%", "%abc%"),
        ("hello world", "%hello world%", "%hello world%"),
        ("a%b", "%a\\%b%", "%"),
        ("%abc%", "%\\%abc\\%%", "%abc%"),
        ("abc%def", "%abc\\%def%", "%abc%def%"),
        ("ab%cd", "%ab\\%cd%", "%"),
        ("a_b", "%a\\_b%", "%"),
        ("abc_def", "%abc\\_def%", "%abc%def%"),
        ("abc\\%def", "%abc\\\\\\\\\\%def%", "%abc\\%def%"),
        ("abc\\_def", "%abc\\\\\\\\\\_def%", "%abc\\_def%"),
        ("%", "%", "%"),
        ("__", "%", "%"),
        ("café", "%café%", "%café%"),
        ("中文字", "%中文字%", "%中文字%"),
        (
            "quote\"inside",
            "%quote\\\\\"inside%",
            "%quote\\\\\"inside%",
        ),
        ("tab\there", "%tab\\\\there%", "%tab\\\\there%"),
        ("new\nline", "%new\\\\nline%", "%new\\\\nline%"),
        ("trailing\\", "%trailing\\\\\\\\%", "%"),
        ("100%", "%100\\%%", "%100%"),
        ("50_off", "%50\\_off%", "%off%"),
        ("x%yz%abcdef", "%x\\%yz\\%abcdef%", "%abcdef%"),
        ("zc\na中中\n", "%zc\\\\na中中\\\\n%", "%zc\\\\na中中%"),
        (
            "\\\tzbcb\\\n",
            "%\\\\\\\\\\\\tzbcb\\\\\\\\\\\\n%",
            "%\\\\tzbcb\\\\n%",
        ),
        ("abcdef_gh\\", "%abcdef\\_gh\\\\\\\\%", "%abcdef%"),
        ("\\中b\n", "%\\\\\\\\中b\\\\n%", "%"),
        (
            "\"xc\t\nzé_\\",
            "%\\\\\"xc\\\\t\\\\nzé\\_\\\\\\\\%",
            "%\\\\\"xc\\\\t\\\\nzé%",
        ),
        ("abc\\\\def", "%abc\\\\\\\\\\\\\\\\def%", "%abc\\\\\\\\def%"),
    ];

    #[test]
    fn the_port_agrees_with_odoo_on_every_recorded_pair() {
        for (input, want_value, want_pattern) in PAIRS {
            assert_eq!(&value_to_pattern(input), want_value, "value({input:?})");
            assert_eq!(
                &pattern_to_pattern(input),
                want_pattern,
                "pattern({input:?})"
            );
        }
    }

    #[test]
    fn a_short_run_cannot_constrain_the_index_and_is_dropped() {
        // Two characters is below a trigram, so the conjunct must widen to
        // `%` rather than demand a substring the base condition does not.
        assert_eq!(pattern_to_pattern("ab"), "%");
        assert_eq!(value_to_pattern("ab"), "%");
    }

    #[test]
    fn the_conjunct_never_narrows_past_the_literal_runs() {
        // Every kept run appears in the pattern surrounded by `%`, so the
        // conjunct is implied by the LIKE it accompanies.
        assert_eq!(pattern_to_pattern("%abcd%efgh%"), "%abcd%efgh%");
        assert_eq!(pattern_to_pattern("abcd_efgh"), "%abcd%efgh%");
    }
}
