use super::*;

#[test]
fn validator_catches_mixed_sibling_indents_and_block_under_flow_map() {
    let mixed_indent = "providers:\n    openai:\n        baseUrl: https://api.openai.com/v1\n  agy:\n    baseUrl: http://127.0.0.1:8787/v1\n";
    assert!(validate_yaml_structure(mixed_indent).is_err());

    let block_under_flow = "tools: {commands: false}\n  approvalMode: yolo\n";
    assert!(validate_yaml_structure(block_under_flow).is_err());

    let valid_four_space = "providers:\n    openai:\n        baseUrl: https://api.openai.com/v1\n    agy:\n        baseUrl: http://127.0.0.1:8787/v1\n        models:\n            - id: gemini-4-carbon-high\n              name: \"Gemini 4 Carbon (High)\"\n";
    assert!(validate_yaml_structure(valid_four_space).is_ok());

    let valid_block_scalar = "systemPrompt: |\n  Always run cargo test before committing.\n  Keep commits focused.\nmodelRoles:\n  default: agy/gemini-4-carbon-high\n";
    assert!(validate_yaml_structure(valid_block_scalar).is_ok());
}

#[test]
fn parses_unquoted_and_quoted_yaml_flow_lists() {
    assert_eq!(
        parse_yaml_flow_string_list("[./.omp/user-ext.ts]").unwrap(),
        vec!["./.omp/user-ext.ts".to_owned()]
    );
    assert_eq!(
        parse_yaml_flow_string_list("['./.omp/user-ext.ts', \"./.omp/second.ts\"]").unwrap(),
        vec![
            "./.omp/user-ext.ts".to_owned(),
            "./.omp/second.ts".to_owned()
        ]
    );
    assert!(parse_yaml_flow_string_list("[./unclosed.ts").is_err());
}
