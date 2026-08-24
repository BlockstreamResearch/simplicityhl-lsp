use super::*;

#[test]
fn validates_witness_shape_and_required_fields() {
    let cases = [
        ("[]", "Witness file must be a JSON object"),
        (
            r#"{"amount": 1}"#,
            "Witness 'amount' must be an object with 'value' and 'type' fields",
        ),
        (
            r#"{"amount":{"type":"u32"}}"#,
            "Witness 'amount' is missing required 'value' field",
        ),
        (
            r#"{"amount":{"value":1}}"#,
            "Witness 'amount' is missing required 'type' field",
        ),
    ];

    for (text, expected) in cases {
        assert_eq!(validate(text)[0].message, expected);
    }
    assert!(validate(r#"{"amount":{"value":1,"type":"u32"}}"#).is_empty());
}

#[test]
fn key_diagnostics_use_utf16_columns() {
    let text = r#"{"😀":{"value":1,"type":"u32"},"amount":{"value":1}}"#;
    let diagnostic = validate(text)
        .into_iter()
        .find(|item| item.message.contains("amount"))
        .expect("missing type diagnostic");

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(0, 31), Position::new(0, 31))
    );
}

#[test]
fn key_diagnostics_point_to_top_level_escaped_keys() {
    let text = r#"{"first":{"value":"a\"😀","type":"str"},"a\"😀":{"value":1}}"#;
    let key_start = text.rfind(r#""a\"😀""#).expect("top-level key");
    let expected_column = text[..key_start].encode_utf16().count();
    let diagnostic = validate(text)
        .into_iter()
        .find(|item| item.message.contains(r#"a"😀"#))
        .expect("missing type diagnostic");

    assert_eq!(
        diagnostic.range,
        Range::new(
            Position::new(0, u32::try_from(expected_column).unwrap()),
            Position::new(0, u32::try_from(expected_column).unwrap()),
        )
    );
}

#[test]
fn duplicate_decoded_keys_point_to_the_surviving_member() {
    let text = r#"{"\u0061":{"value":1,"type":"u32"},"a":{"value":1}}"#;
    let key_start = text.rfind(r#""a""#).expect("surviving key");
    let expected_column = text[..key_start].encode_utf16().count();
    let diagnostic = validate(text)
        .into_iter()
        .find(|item| item.message.contains("Witness 'a'"))
        .expect("missing type diagnostic");

    assert_eq!(
        diagnostic.range,
        Range::new(
            Position::new(0, u32::try_from(expected_column).unwrap()),
            Position::new(0, u32::try_from(expected_column).unwrap()),
        )
    );
}

#[test]
fn syntax_diagnostics_use_utf16_columns_and_json_lines() {
    let diagnostic = &validate("{\n  \"😀\": 1, ]\n}")[0];

    assert!(diagnostic.message.starts_with("JSON syntax error:"));
    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(1, 11), Position::new(1, 12))
    );
}
