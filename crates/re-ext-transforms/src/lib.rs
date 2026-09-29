use anyhow::{Context, Result, bail};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};
use heck::{ToKebabCase, ToLowerCamelCase, ToSnakeCase, ToTitleCase, ToUpperCamelCase};

#[derive(Clone, Copy)]
pub enum CaseTransform {
    Lower,
    Upper,
    Proper,
    Camel,
    Pascal,
    Snake,
    Kebab,
}

pub fn convert_case(text: &str, transform: CaseTransform) -> String {
    map_lines(text, |line| match transform {
        CaseTransform::Lower => line.to_lowercase(),
        CaseTransform::Upper => line.to_uppercase(),
        CaseTransform::Proper => line.to_title_case(),
        CaseTransform::Camel => line.to_lower_camel_case(),
        CaseTransform::Pascal => line.to_upper_camel_case(),
        CaseTransform::Snake => line.to_snake_case(),
        CaseTransform::Kebab => line.to_kebab_case(),
    })
}

pub fn decode_base64(text: &str) -> Result<String> {
    let compact: String = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if compact.is_empty() {
        bail!("the input is empty");
    }

    let decoded = [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD]
        .into_iter()
        .find_map(|engine| engine.decode(compact.as_bytes()).ok())
        .context("the input is not valid standard or URL-safe Base64")?;

    String::from_utf8(decoded).context("the decoded bytes are not valid UTF-8 text")
}

pub fn pretty_json(text: &str) -> Result<String> {
    let mut value: serde_json::Value =
        serde_json::from_str(text).context("the input is not valid JSON")?;
    value.sort_all_objects();
    serde_json::to_string_pretty(&value).context("JSON formatting failed")
}

fn map_lines(text: &str, transform: impl Fn(&str) -> String) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let (content, ending) = if let Some(content) = line.strip_suffix("\r\n") {
                (content, "\r\n")
            } else if let Some(content) = line.strip_suffix('\n') {
                (content, "\n")
            } else {
                (line, "")
            };
            transform(content) + ending
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{CaseTransform, convert_case, decode_base64, pretty_json};

    #[test]
    fn converts_common_identifier_cases() {
        assert_eq!(
            convert_case("hello sample WORLD", CaseTransform::Camel),
            "helloSampleWorld"
        );
        assert_eq!(
            convert_case("hello sample WORLD", CaseTransform::Proper),
            "Hello Sample World"
        );
        assert_eq!(
            convert_case("hello sample WORLD", CaseTransform::Snake),
            "hello_sample_world"
        );
    }

    #[test]
    fn decodes_standard_and_url_safe_base64() {
        assert_eq!(decode_base64("SGVsbG8=").expect("standard Base64"), "Hello");
        assert_eq!(decode_base64("SGVsbG8").expect("unpadded Base64"), "Hello");
    }

    #[test]
    fn formats_json() {
        assert_eq!(
            pretty_json(r#"{"enabled":true,"count":2}"#).expect("valid JSON"),
            "{\n  \"count\": 2,\n  \"enabled\": true\n}"
        );
    }
}
