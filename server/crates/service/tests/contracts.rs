use serde::Deserialize;
use serde_json::{Value, json};
use turing_eye_service::contracts::CreatePlanRequest;

const OPENAPI: &str = include_str!("../../../../contracts/openapi.json");
const EXAMPLES: &str = include_str!("../../../../contracts/examples.json");
const TOOLS: &str = include_str!("../../../../contracts/tool-protocol.schema.json");

#[derive(Deserialize)]
struct Case {
    name: String,
    schema: String,
    expect: String,
    error_code: Option<String>,
    value: Value,
}

#[derive(Deserialize)]
struct Cases {
    cases: Vec<Case>,
}

#[test]
fn all_http_components_and_golden_cases_are_consumable() {
    let document: Value = serde_json::from_str(OPENAPI).unwrap();
    let components = document["components"].clone();
    for name in components["schemas"].as_object().unwrap().keys() {
        let schema =
            json!({"$ref": format!("#/components/schemas/{name}"), "components": components});
        jsonschema::validator_for(&schema).unwrap_or_else(|error| panic!("{name}: {error}"));
    }
    let cases: Cases = serde_json::from_str(EXAMPLES).unwrap();
    assert_eq!(cases.cases.len(), 43);
    for case in cases.cases {
        let schema = json!({"$ref": format!("#/components/schemas/{}", case.schema), "components": components});
        let validator = jsonschema::validator_for(&schema).unwrap();
        let valid = validator.is_valid(&case.value);
        assert_eq!(
            valid,
            case.expect != "schema_invalid",
            "{}: {:?}",
            case.name,
            validator
                .iter_errors(&case.value)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
        if case.schema == "CreatePlanRequest" && case.expect == "business_invalid" {
            let parsed: CreatePlanRequest = serde_json::from_value(case.value.clone()).unwrap();
            let issues = parsed.validate_dimensions().unwrap_err();
            assert!(
                issues
                    .iter()
                    .any(|issue| serde_json::to_value(issue.code).unwrap().as_str()
                        == case.error_code.as_deref()),
                "{}: {issues:?}",
                case.name
            );
        }
        if case.schema == "CreatePlanRequest" && case.expect == "schema_valid" {
            let parsed: CreatePlanRequest = serde_json::from_value(case.value.clone()).unwrap();
            let validated = parsed.validate_dimensions().unwrap();
            let expected = case.value["dimensions"].as_array().unwrap();
            assert_eq!(
                serde_json::to_value(validated.dimensions()).unwrap(),
                case.value["dimensions"]
            );
            assert_eq!(parsed.dimensions.len(), expected.len());
            for (actual, original) in parsed.dimensions.iter().zip(expected) {
                assert_eq!(serde_json::to_value(actual).unwrap(), *original);
            }
        }
    }
}

#[test]
fn all_browser_frames_validate_and_reject_unknown_fields() {
    let schema: Value = serde_json::from_str(TOOLS).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let frames = schema["examples"].as_array().unwrap();
    assert_eq!(frames.len(), 18);
    for frame in frames {
        assert!(
            validator.is_valid(frame),
            "{:?}",
            validator
                .iter_errors(frame)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
        let mut invalid = frame.clone();
        invalid["unexpected_permission"] = json!("shell");
        assert!(!validator.is_valid(&invalid));
    }
}
