//! Template rendering shared by log events, command responses, and plan steps.

use std::collections::BTreeMap;

/// Renders a template, replacing `{variable}` tokens with values from `vars`.
/// Unknown tokens are left untouched so template mistakes are visible.
pub fn render(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{'
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '}')
        {
            let key: String = chars[i + 1..i + 1 + end].iter().collect();
            if let Some(v) = vars.get(&key) {
                out.push_str(v);
                i += end + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Render a Discord-sized report without first allocating an expanded message.
/// The limit counts UTF-16 units; unknown tokens retain the normal renderer's behavior.
pub fn render_bounded(template: &str, vars: &BTreeMap<String, String>, max_units: usize) -> String {
    let mut out = String::new();
    let mut remaining = max_units;
    let mut append = |text: &str| {
        for ch in text.chars() {
            if ch.len_utf16() > remaining {
                return false;
            }
            out.push(ch);
            remaining -= ch.len_utf16();
        }
        true
    };
    for chunk in template.split_inclusive('}') {
        let complete = if chunk.ends_with('}') {
            if let Some(start) = chunk.rfind('{') {
                if let Some(value) = vars.get(&chunk[start + 1..chunk.len() - 1]) {
                    append(&chunk[..start]) && append(value)
                } else {
                    append(chunk)
                }
            } else {
                append(chunk)
            }
        } else {
            append(chunk)
        };
        if !complete {
            if max_units > 0 {
                // Make room for the marker even if the previous character used two units.
                if remaining == 0 {
                    out.pop();
                }
                out.push('…');
            }
            break;
        }
    }
    out
}

/// Builds the standard variable map for a user.
pub fn user_vars(user_id: u64, username: &str, display_name: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("user".into(), display_name.to_string());
    m.insert("user_id".into(), user_id.to_string());
    m.insert("mention".into(), format!("<@{}>", user_id));
    m.insert("username".into(), username.to_string());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_known_and_keeps_unknown() {
        let mut vars = BTreeMap::new();
        vars.insert("user".into(), "alice".into());
        let out = render("hi {user}, {missing}!", &vars);
        assert_eq!(out, "hi alice, {missing}!");
    }

    #[test]
    fn nested_braces_ignored() {
        let out = render("literal {", &BTreeMap::new());
        assert_eq!(out, "literal {");
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    #[test]
    fn bounded_render_preserves_template_semantics() {
        let vars = BTreeMap::from([("user".into(), "alice {unknown}".into())]);
        for template in [
            "hi {user}",
            "{{user}}",
            "{unknown} {user}",
            "literal {",
            "{outer {user} tail",
        ] {
            assert_eq!(
                render_bounded(template, &vars, 2000),
                render(template, &vars)
            );
        }
    }
    #[test]
    fn oversized_substitutions_stop_at_unicode_budget() {
        let vars = BTreeMap::from([("body".into(), "🦀".repeat(10_000))]);
        for limit in [0, 1, 2, 3, 1999, 2000] {
            let out = render_bounded("{body}{body}{body}", &vars, limit);
            assert!(out.encode_utf16().count() <= limit);
            if limit > 0 {
                assert!(out.ends_with('…'));
            }
        }
    }
}
