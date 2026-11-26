use uuid::Uuid;

use crate::yaml_edit::validate_yaml_structure;
use crate::{DiscoveredModel, HarnessSyncParams};

use super::*;

fn sample_params() -> HarnessSyncParams {
    HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-3.8-flash-low".to_owned(),
        no_bypass: false,
        no_code_mode: false,
        discovered_models: vec![DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: "Gemini 4 Carbon (High)".to_owned(),
            context_window: 500_000,
            auto_compact_token_limit: 425_000,
        }],
    }
}

#[test]
fn omp_four_space_models_and_config_roundtrip_preserves_indentation_and_comments() {
    let tmp = std::env::temp_dir().join(format!("lg-omp-4space-{}", Uuid::new_v4()));
    let agent_dir = tmp.join("agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let ledger = tmp.join("ledger.json");
    let paths = OmpResolvedPaths {
        config_path: agent_dir.join("config.yml"),
        auxiliary_path: agent_dir.join("models.yml"),
    };

    let initial_models = "\
# User 4-space models.yml
providers:
    ollama:
        baseUrl: http://127.0.0.1:11434/v1
        api: openai-completions
";
    let initial_config = "\
# User 4-space config.yml
inlineToolDescriptors: auto # user default
modelRoles:
    default: ollama/qwen3
    advisor: openai/o3 # keep advisor
tools: {commands: false, customSandbox: strict} # inline flow map
";
    fs::write(&paths.auxiliary_path, initial_models).unwrap();
    fs::write(&paths.config_path, initial_config).unwrap();

    let params = sample_params();
    write_or_update_omp_config_with_ledger(Some(&ledger), true, &paths, &params, false).unwrap();

    let synced_models = fs::read_to_string(&paths.auxiliary_path).unwrap();
    let synced_cfg = fs::read_to_string(&paths.config_path).unwrap();
    validate_yaml_structure(&synced_models).unwrap();
    validate_yaml_structure(&synced_cfg).unwrap();

    assert!(
        synced_models.contains("\n    agy:\n        baseUrl:"),
        "agy provider must match existing 4-space providers mapping:\n{synced_models}"
    );
    assert!(has_omp_agy_provider_yaml(&synced_models));
    assert!(
        synced_cfg.contains("\n    default: agy/gemini-4-carbon-high\n"),
        "modelRoles must preserve 4-space child indentation:\n{synced_cfg}"
    );
    assert!(synced_cfg.contains("customSandbox: strict"));
    assert!(synced_cfg.contains("commands: true"));
    assert!(synced_cfg.contains("approvalMode: yolo"));
    assert!(synced_cfg.contains("inlineToolDescriptors: \"off\" # user default"));
    assert!(synced_cfg.contains("evalNamespaces: true"));
    assert_eq!(
        inspect_omp_default_model(&synced_cfg).as_deref(),
        Some("agy/gemini-4-carbon-high")
    );

    let removed = uninstall_omp_config_with_ledger(Some(&ledger), true, &paths).unwrap();
    assert!(removed);

    let restored_models = fs::read_to_string(&paths.auxiliary_path).unwrap();
    let restored_cfg = fs::read_to_string(&paths.config_path).unwrap();
    validate_yaml_structure(&restored_models).unwrap();
    validate_yaml_structure(&restored_cfg).unwrap();
    assert!(!has_omp_agy_provider_yaml(&restored_models));
    assert!(restored_models.contains("    ollama:\n"));
    assert!(restored_cfg.contains("    default: ollama/qwen3\n"));
    assert!(restored_cfg.contains("    advisor: openai/o3 # keep advisor\n"));
    assert!(restored_cfg.contains("commands: false"));
    assert!(restored_cfg.contains("customSandbox: strict"));
    assert!(restored_cfg.contains("inlineToolDescriptors: auto # user default"));
    assert!(!restored_cfg.contains("evalNamespaces:"));

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn omp_flow_list_extensions_preserved_across_sync_and_uninstall_and_invalid_yaml_rejected() {
    let tmp = std::env::temp_dir().join(format!("lg-omp-flowlist-{}", Uuid::new_v4()));
    let ledger = tmp.join("ledger.json");

    for initial_ext in [
        "extensions: [./.omp/user-ext.ts]\n",
        "extensions: ['./.omp/user-ext.ts', \"./.omp/second-ext.ts\"]\n",
    ] {
        let proj = tmp.join(format!("proj-{}", Uuid::new_v4()));
        fs::create_dir_all(proj.join(".omp")).unwrap();
        let paths = resolve_omp_paths(false, Some(&proj)).unwrap();
        fs::write(&paths.config_path, initial_ext).unwrap();

        let extracted = extract_omp_yaml_extensions(initial_ext);
        assert!(extracted.contains(&"./.omp/user-ext.ts".to_owned()));

        write_or_update_omp_config_with_ledger(
            Some(&ledger),
            false,
            &paths,
            &sample_params(),
            false,
        )
        .unwrap();

        let synced = fs::read_to_string(&paths.config_path).unwrap();
        validate_yaml_structure(&synced).unwrap();
        assert!(synced.contains("./.omp/user-ext.ts"));
        assert!(synced.contains(OMP_LOCAL_EXTENSION_REL_PATH));

        let removed = uninstall_omp_config_with_ledger(Some(&ledger), false, &paths).unwrap();
        assert!(removed);
        let restored = fs::read_to_string(&paths.config_path).unwrap();
        validate_yaml_structure(&restored).unwrap();
        assert!(restored.contains("./.omp/user-ext.ts"));
        assert!(!restored.contains(OMP_LOCAL_EXTENSION_REL_PATH));
    }

    // Unsupported inline nested flow map in `models.yml` must fail closed without touching disk.
    let bad_proj = tmp.join("bad-models");
    fs::create_dir_all(&bad_proj).unwrap();
    let bad_paths = OmpResolvedPaths {
        config_path: bad_proj.join("config.yml"),
        auxiliary_path: bad_proj.join("models.yml"),
    };
    let original_flow_models = "providers: { ollama: { baseUrl: 'http://127.0.0.1:11434/v1' } }\n";
    fs::write(&bad_paths.auxiliary_path, original_flow_models).unwrap();
    let err = write_or_update_omp_config_with_ledger(
        Some(&ledger),
        true,
        &bad_paths,
        &sample_params(),
        false,
    );
    assert!(matches!(
        err,
        Err(HarnessError::UnsupportedYamlLayout { .. })
    ));
    assert_eq!(
        fs::read_to_string(&bad_paths.auxiliary_path).unwrap(),
        original_flow_models,
        "rejected YAML layout must leave user's file untouched"
    );

    let _ = fs::remove_dir_all(&tmp);
}
