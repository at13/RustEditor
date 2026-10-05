//! Declarative recipes: text transforms the editor runs itself. Sandboxed
//! by construction (no plugin code runs) and need no toolchain.

use anyhow::{Context, Result, bail};
use regex::{Regex, RegexBuilder};
use serde_json::Value;

/// Most steps in one recipe command.
const MAX_STEPS: usize = 32;

#[derive(Clone, Debug)]
pub enum Step {
    RegexReplace { regex: Regex, replacement: String },
    SortLines,
    SortLinesDescending,
    UniqueLines,
    ReverseLines,
    TrimTrailingWhitespace,
    Case(re_ext_transforms::CaseTransform),
    JsonFormat,
    Base64Decode,
}

impl PartialEq for Step {
    fn eq(&self, other: &Self) -> bool {
        format!("{self:?}") == format!("{other:?}")
    }
}

impl Eq for Step {}

/// Every step name a recipe may use (for docs and validation messages).
pub const STEP_NAMES: &[&str] = &[
    "regex_replace",
    "sort_lines",
    "sort_lines_descending",
    "unique_lines",
    "reverse_lines",
    "trim_trailing_whitespace",
    "lowercase",
    "uppercase",
    "proper_case",
    "camel_case",
    "pascal_case",
    "snake_case",
    "kebab_case",
    "json_format",
    "base64_decode",
];

pub fn parse_steps(value: Option<&Value>, command: &str) -> Result<Vec<Step>> {
    let steps = value
        .and_then(Value::as_array)
        .with_context(|| format!("command `{command}`: a recipe needs a `steps` array"))?;
    if steps.is_empty() || steps.len() > MAX_STEPS {
        bail!("command `{command}`: a recipe needs 1-{MAX_STEPS} steps");
    }
    steps.iter().map(|step| parse_step(step, command)).collect()
}

fn parse_step(step: &Value, command: &str) -> Result<Step> {
    use re_ext_transforms::CaseTransform as C;
    if let Some(name) = step.as_str() {
        return Ok(match name {
            "sort_lines" => Step::SortLines,
            "sort_lines_descending" => Step::SortLinesDescending,
            "unique_lines" => Step::UniqueLines,
            "reverse_lines" => Step::ReverseLines,
            "trim_trailing_whitespace" => Step::TrimTrailingWhitespace,
            "lowercase" => Step::Case(C::Lower),
            "uppercase" => Step::Case(C::Upper),
            "proper_case" => Step::Case(C::Proper),
            "camel_case" => Step::Case(C::Camel),
            "pascal_case" => Step::Case(C::Pascal),
            "snake_case" => Step::Case(C::Snake),
            "kebab_case" => Step::Case(C::Kebab),
            "json_format" => Step::JsonFormat,
            "base64_decode" => Step::Base64Decode,
            "regex_replace" => {
                bail!("command `{command}`: `regex_replace` needs an object with `pattern`")
            }
            other => bail!(
                "command `{command}`: unknown step `{other}` (use one of: {})",
                STEP_NAMES.join(", ")
            ),
        });
    }
    let object = step
        .as_object()
        .filter(|o| o.len() == 1)
        .with_context(|| format!("command `{command}`: a step is a name or a one-key object"))?;
    let (name, args) = object.iter().next().expect("one key");
    if name != "regex_replace" {
        bail!("command `{command}`: only `regex_replace` takes arguments (got `{name}`)");
    }
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .with_context(|| format!("command `{command}`: regex_replace needs `pattern`"))?;
    let replacement = args
        .get("replacement")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(flag("case_insensitive"))
        .multi_line(flag("multiline"))
        // `$` and `^` treat CRLF as one line break (Windows files).
        .crlf(true)
        .size_limit(1 << 20)
        .build()
        .with_context(|| format!("command `{command}`: invalid regex `{pattern}`"))?;
    Ok(Step::RegexReplace { regex, replacement })
}

/// Applies `steps` to one input.
pub fn apply(steps: &[Step], input: &str) -> Result<String> {
    let mut text = input.to_owned();
    for step in steps {
        text = match step {
            Step::RegexReplace { regex, replacement } => {
                regex.replace_all(&text, replacement.as_str()).into_owned()
            }
            Step::SortLines => map_lines(&text, |lines| lines.sort_unstable()),
            Step::SortLinesDescending => {
                map_lines(&text, |lines| lines.sort_unstable_by(|a, b| b.cmp(a)))
            }
            Step::UniqueLines => map_lines(&text, |lines| {
                let mut seen = std::collections::HashSet::new();
                lines.retain(|line| seen.insert(line.to_owned()));
            }),
            Step::ReverseLines => map_lines(&text, |lines| lines.reverse()),
            Step::TrimTrailingWhitespace => map_lines(&text, |lines| {
                for line in lines.iter_mut() {
                    *line = line.trim_end();
                }
            }),
            Step::Case(case) => re_ext_transforms::convert_case(&text, *case),
            Step::JsonFormat => re_ext_transforms::pretty_json(&text)?,
            Step::Base64Decode => re_ext_transforms::decode_base64(&text)?,
        };
    }
    Ok(text)
}

/// Runs `f` on the lines, keeping the text's line ending and whether it
/// ended with a line break.
fn map_lines(text: &str, f: impl FnOnce(&mut Vec<&str>)) -> String {
    let ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing = text.ends_with('\n');
    let mut lines: Vec<&str> = text.lines().collect();
    f(&mut lines);
    let mut out = lines.join(ending);
    if trailing {
        out.push_str(ending);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn steps_parse_and_apply_in_order() {
        let steps = parse_steps(
            Some(&json!([
                {"regex_replace": {"pattern": "[ \\t]+$", "replacement": "", "multiline": true}},
                "sort_lines",
                "unique_lines",
                "uppercase"
            ])),
            "c",
        )
        .unwrap();
        assert_eq!(apply(&steps, "b  \r\na\r\nb\r\n").unwrap(), "A\r\nB\r\n");
        let trim = parse_steps(
            Some(&json!(["trim_trailing_whitespace", "reverse_lines"])),
            "c",
        )
        .unwrap();
        assert_eq!(apply(&trim, "x \ny\t\n").unwrap(), "y\nx\n");
        let json_step = parse_steps(Some(&json!(["json_format"])), "c").unwrap();
        assert!(apply(&json_step, "{bad").is_err());
    }

    #[test]
    fn bad_recipes_are_rejected_with_reasons() {
        let cases = [
            (json!(null), "needs a `steps` array"),
            (json!([]), "1-32 steps"),
            (json!(["explode"]), "unknown step `explode`"),
            (json!(["regex_replace"]), "needs an object"),
            (
                json!([{"regex_replace": {"pattern": "("}}]),
                "invalid regex",
            ),
            (json!([{"sort_lines": {}}]), "only `regex_replace`"),
            (json!([{"regex_replace": {}}]), "needs `pattern`"),
        ];
        for (value, needle) in cases {
            let error = format!("{:#}", parse_steps(Some(&value), "c").unwrap_err());
            assert!(error.contains(needle), "{value}: {error}");
        }
    }
}
