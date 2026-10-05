//! System prompt templates: `{{name}}` placeholders filled by the host.

/// Replaces each `{{key}}` with its value; unknown placeholders become "".
pub fn fill(template: &str, vars: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = after[..end].trim();
        if let Some((_, value)) = vars.iter().find(|(k, _)| *k == key) {
            out.push_str(value);
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Removes `<!-- ... -->` comments (authoring notes in prompt files).
pub fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_placeholders_and_strips_comments() {
        let text = fill(
            "File {{ name }} has {{lines}} lines{{missing}}.",
            &[("name", "a.rs".into()), ("lines", "3".into())],
        );
        assert_eq!(text, "File a.rs has 3 lines.");
        assert_eq!(fill("open {{ only", &[]), "open {{ only");
        assert_eq!(strip_comments("a<!-- note -->b<!-- x"), "ab");
    }
}
