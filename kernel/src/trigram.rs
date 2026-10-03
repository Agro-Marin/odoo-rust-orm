fn json_escape(s: &str) -> String {
    let quoted = serde_json::Value::String(s.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

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

/// Whether this port's patterns are exact for `term`. Odoo's prefilter now
/// unaccents each literal run with the database's own table and, where a `"`
/// or `\` may stand for a stored character unaccent folded into it (″ “ ＼),
/// ORs both readings (odoo 9b42923793ab). This port has no unaccent table: it
/// matches Odoo only on plain ASCII without quotes, backslashes or control
/// characters, which unaccent leaves alone and JSON writes unchanged. Any
/// other term gets no prefilter -- slower, never wrong.
pub fn is_exact_on(term: &str) -> bool {
    term.chars()
        .all(|c| c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\')
}

pub fn value_to_pattern(value: &str) -> String {
    if value.chars().count() < 3 {
        return "%".into();
    }
    format!("%{}%", escape_wildcards(&json_escape(value)))
}

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
            '\\' => {
                dangling = true;
                i += 1;
            }
            '_' | '%' => {
                segments.push(std::mem::take(&mut cur));
                i += 1;
            }
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
    fn a_term_unaccent_or_json_could_rewrite_is_not_exact() {
        for term in [
            "1/2\"",
            "x\u{201c}Galv",
            "ab\\cd",
            "ab\u{ff3c}cd",
            "caf\u{e9}",
            "a\tb",
        ] {
            assert!(!is_exact_on(term), "{term:?}");
        }
        for term in ["Tubo PVC", "50_off", "x%yz%abcdef", "O'Brien"] {
            assert!(is_exact_on(term), "{term:?}");
        }
    }

    #[test]
    fn a_short_run_cannot_constrain_the_index_and_is_dropped() {
        assert_eq!(pattern_to_pattern("ab"), "%");
        assert_eq!(value_to_pattern("ab"), "%");
    }

    #[test]
    fn the_conjunct_never_narrows_past_the_literal_runs() {
        assert_eq!(pattern_to_pattern("%abcd%efgh%"), "%abcd%efgh%");
        assert_eq!(pattern_to_pattern("abcd_efgh"), "%abcd%efgh%");
    }
}
