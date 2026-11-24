use std::{fs, sync::Arc};

use mcp_orca::{
    config::Config,
    tools::{ToolContext, ToolRegistry},
};
use serde_json::{json, Value};
use tempfile::tempdir;

fn write_json(path: &std::path::Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn structured(result: rust_mcp_sdk::schema::CallToolResult) -> Value {
    Value::Object(result.structured_content.unwrap_or_default())
}

async fn call(registry: &ToolRegistry, ctx: &Arc<ToolContext>, tool: &str, args: Value) -> Value {
    let result = registry.call(ctx, tool, Some(args)).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    structured(result)
}

fn local_config(td: &tempfile::TempDir) -> std::path::PathBuf {
    let root = td.path().join("orca");
    let machine = root.join("user/default/machine");
    let filament = root.join("user/default/filament");
    let process = root.join("user/default/process");
    fs::create_dir_all(&machine).unwrap();
    fs::create_dir_all(&filament).unwrap();
    fs::create_dir_all(&process).unwrap();

    write_json(
        &machine.join("base.json"),
        &json!({
            "name": "Base Printer",
            "nozzle_diameter": ["0.4"],
            "nozzle_type": ["brass"],
            "printable_area": ["0x0", "220x0", "220x220", "0x220"],
            "printable_height": 250.0,
            "gcode_flavor": "klipper",
            "machine_start_gcode": "G28\n",
            "machine_end_gcode": "M84\n"
        }),
    );
    write_json(
        &machine.join("local.json"),
        &json!({
            "name": "Local Printer",
            "inherits": "Base Printer",
            "nozzle_diameter": ["0.6"]
        }),
    );
    write_json(
        &filament.join("base.json"),
        &json!({
            "name": "Base Filament",
            "filament_type": ["PLA"],
            "filament_diameter": [1.75],
            "filament_density": [1.24],
            "nozzle_temperature": ["215"],
            "hot_plate_temp": ["55"]
        }),
    );
    write_json(
        &filament.join("local.json"),
        &json!({
            "name": "Local Filament",
            "inherits": "Base Filament",
            "nozzle_temperature": ["225"]
        }),
    );
    write_json(
        &process.join("local.json"),
        &json!({
            "name": "Local Process",
            "layer_height": 0.2,
            "wall_loops": 3,
            "sparse_infill_density": 15.0,
            "sparse_infill_pattern": "crosshatch",
            "outer_wall_speed": 120
        }),
    );
    write_json(
        &root.join("OrcaSlicer.conf"),
        &json!({
            "presets": {"machine": "Local Printer"},
            "orca_presets": [{
                "machine": "Local Printer",
                "filament": "Local Filament",
                "process": "Local Process"
            }]
        }),
    );
    root
}

#[tokio::test]
async fn scans_resolves_and_records_local_defaults() {
    let td = tempdir().unwrap();
    let config_dir = local_config(&td);

    let mut cfg = Config::default();
    cfg.workspace.root = td.path().join("ws").to_string_lossy().to_string();
    cfg.profiles.use_os_defaults = false;
    cfg.profiles.search_paths = vec![];

    let ctx = Arc::new(ToolContext::new(cfg).unwrap());
    let registry = ToolRegistry::new();
    let dir_arg = config_dir.to_string_lossy().to_string();

    // Scan: finds user presets and the active OrcaSlicer selection.
    let scan = call(
        &registry,
        &ctx,
        "orca_config_scan",
        json!({"config_dir": dir_arg}),
    )
    .await;
    assert_eq!(scan["counts"]["printers"], 2);
    assert_eq!(scan["counts"]["filaments"], 2);
    assert_eq!(scan["counts"]["processes"], 1);
    assert_eq!(scan["current_selection"]["printer"], "Local Printer");
    assert_eq!(scan["defaults"]["configured"], false);

    // List only printers matching a substring.
    let list = call(
        &registry,
        &ctx,
        "orca_config_profiles",
        json!({"config_dir": dir_arg, "kind": "printer", "filter": "local"}),
    )
    .await;
    assert_eq!(list["matched"], 1);
    assert_eq!(list["profiles"][0]["name"], "Local Printer");
    assert_eq!(list["profiles"][0]["inherits"], "Base Printer");

    // Resolve a single preset through its inheritance chain.
    let resolved = call(
        &registry,
        &ctx,
        "orca_config_resolve",
        json!({
            "config_dir": dir_arg,
            "kind": "printer",
            "name": "Local Printer",
            "include_settings": true
        }),
    )
    .await;
    assert_eq!(resolved["preset"]["found"], true);
    assert_eq!(resolved["preset"]["setting_count"], 7);
    assert_eq!(resolved["settings"]["nozzle_diameter"], json!(["0.6"]));
    assert_eq!(resolved["settings"]["gcode_flavor"], "klipper");
    assert_eq!(
        resolved["essentials"]["roles"]["printer"]["missing_required"],
        json!([])
    );
    assert_eq!(
        resolved["essentials"]["roles"]["printer"]["settings"]["machine_start_gcode"],
        "G28\n"
    );

    // First-time setup uses the detected OrcaSlicer selection.
    let setup = call(
        &registry,
        &ctx,
        "orca_config_setup",
        json!({"config_dir": dir_arg}),
    )
    .await;
    assert_eq!(setup["saved"], true);
    assert_eq!(setup["source"], "detected");
    assert_eq!(setup["defaults"]["selection"]["printer"], "Local Printer");
    assert_eq!(setup["defaults"]["selection"]["filament"], "Local Filament");
    assert_eq!(setup["defaults"]["selection"]["process"], "Local Process");
    assert_eq!(
        setup["essentials"]["roles"]["filament"]["settings"]["nozzle_temperature"],
        json!(["225"])
    );

    // Defaults are now complete and readable.
    let defaults = call(&registry, &ctx, "orca_config_defaults", json!({})).await;
    assert_eq!(defaults["configured"], true);
    assert_eq!(defaults["complete"], true);
    assert_eq!(defaults["selection"]["process"], "Local Process");
    assert!(
        defaults["essentials_summary"]["roles"]["printer"]["setting_count"]
            .as_u64()
            .unwrap()
            > 0
    );

    // A full selection resolve merges all roles.
    let selection = call(
        &registry,
        &ctx,
        "orca_config_resolve",
        json!({
            "config_dir": dir_arg,
            "printer": "Local Printer",
            "filament": "Local Filament",
            "process": "Local Process"
        }),
    )
    .await;
    assert_eq!(selection["missing_roles"], json!([]));
    assert_eq!(
        selection["essentials"]["roles"]["process"]["settings"]["layer_height"],
        0.2
    );
    assert_eq!(
        selection["essentials"]["roles"]["printer"]["settings"]["nozzle_diameter"],
        json!(["0.6"])
    );
}

#[tokio::test]
async fn setup_fills_missing_roles_and_can_reset() {
    let td = tempdir().unwrap();
    let config_dir = local_config(&td);

    let mut cfg = Config::default();
    cfg.workspace.root = td.path().join("ws").to_string_lossy().to_string();
    cfg.profiles.use_os_defaults = false;
    cfg.profiles.search_paths = vec![];

    let ctx = Arc::new(ToolContext::new(cfg).unwrap());
    let registry = ToolRegistry::new();
    let dir_arg = config_dir.to_string_lossy().to_string();

    let setup = call(
        &registry,
        &ctx,
        "orca_config_setup",
        json!({"config_dir": dir_arg, "printer": "Local Printer"}),
    )
    .await;
    assert_eq!(setup["source"], "user");
    assert_eq!(setup["defaults"]["selection"]["filament"], "Local Filament");

    let reset = call(&registry, &ctx, "orca_config_setup", json!({"reset": true})).await;
    assert_eq!(reset["removed"], true);

    let defaults = call(&registry, &ctx, "orca_config_defaults", json!({})).await;
    assert_eq!(defaults["configured"], false);
}

#[tokio::test]
async fn setup_rejects_incomplete_selection_without_detection() {
    let td = tempdir().unwrap();
    let config_dir = local_config(&td);

    let mut cfg = Config::default();
    cfg.workspace.root = td.path().join("ws").to_string_lossy().to_string();
    cfg.profiles.use_os_defaults = false;
    cfg.profiles.search_paths = vec![];

    let ctx = Arc::new(ToolContext::new(cfg).unwrap());
    let registry = ToolRegistry::new();

    let err = registry
        .call(
            &ctx,
            "orca_config_setup",
            Some(json!({
                "config_dir": config_dir.to_string_lossy().to_string(),
                "detect": false
            })),
        )
        .await
        .unwrap_err();
    assert!(err
        .public_message()
        .contains("missing printer, filament, process"));
}
