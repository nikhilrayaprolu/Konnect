//! `design_intent` toolset — a persisted model of engineering intent.
//!
//! Automated placement/routing tools (`placement`, `pcb_routing`) treat a
//! board as a flat optimization problem: N components, M nets, minimize some
//! objective. They have no notion of which components form a functional
//! block, which nets are safety- or signal-integrity-critical, or what a
//! human engineer would actually decide and why. This toolset gives that
//! structure a home: `<project_dir>/.konnect/design_intent.json`, read and
//! written with the exact same convention `config` toolset uses for
//! `project.json` — same directory, same "missing file reads as an empty
//! default, never an error" semantics, same plain `serde_json::Value`
//! read/merge/write shape (see `tools/config.rs`).
//!
//! `phase_gate` (a sibling toolset) reads this file's `functional_blocks` as
//! one of its gate criteria for entering the `placement` phase — see
//! `tools/phase_gate.rs`.
//!
//! ## Schema (`version: 1`)
//!
//! ```json
//! {
//!   "version": 1,
//!   "functional_blocks": {
//!     "<block_id>": { "label": "...", "priority": "critical|high|medium|low",
//!                      "components": ["R1", "C2"], "notes": "..." }
//!   },
//!   "interfaces": {
//!     "<interface_id>": { "kind": "high_speed|differential|analog|power|generic",
//!                          "blocks": ["block_id", ...], "constraints": ["short", ...] }
//!   },
//!   "net_priorities": { "<net_name>": "critical|high|medium|low" },
//!   "decisions": [ { "timestamp": "...", "decision": "...", "reason": "...", "scope": [...] } ]
//! }
//! ```
//!
//! `set_design_intent` replaces the whole document (after shape validation).
//! `update_design_intent` merges individual `functional_blocks` /
//! `interfaces` / `net_priorities` entries by id without touching the rest —
//! most tool-driven usage should go through it, not a full replace.
//! `record_decision` is the only way `decisions` grows: append-only, never
//! edited or replaced by the other two tools.
//!
//! `analyze_design` is the one read-only, non-persisting tool here: it
//! proposes a `functional_blocks` DRAFT for human/agent review and never
//! writes `design_intent.json` itself — the caller decides what to keep by
//! calling `update_design_intent` with some or all of the draft.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, iso8601_now, opt_str, require_str, ToolContext, ToolDef};
use konnect_sexp::board::PcbConnectivityIndex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "get_design_intent",
            "Read the project's persisted design-intent model (functional blocks, interfaces, \
             net priorities, decision log) from <project_dir>/.konnect/design_intent.json. \
             Returns an empty default skeleton — not an error — when no file exists yet.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; its directory is the project directory" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_design_intent(args, ctx).await }
        ),
        tool!(
            "set_design_intent",
            "Replace the ENTIRE design-intent document after validating its shape (priority \
             enums, interface kinds, array/object types). Prefer update_design_intent for \
             adding or changing individual functional blocks / interfaces / net priorities — \
             this tool clobbers everything not present in 'intent', including the decision log \
             if omitted (decisions default to empty; use record_decision to preserve history \
             across a replace by re-reading first).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; its directory is the project directory" },
                    "intent": {
                        "type": "object",
                        "description": "Full design_intent document (see toolset description for schema). Missing top-level keys default to empty."
                    }
                },
                "required": ["board", "intent"]
            }),
            |args, ctx| async move { handle_set_design_intent(args, ctx).await }
        ),
        tool!(
            "update_design_intent",
            "Merge a partial patch into the existing design-intent document without touching \
             anything else. 'patch' may contain 'functional_blocks', 'interfaces', and/or \
             'net_priorities' — each is merged key-by-key (a block/interface/net id in the \
             patch overwrites just that entry; every other existing entry is untouched). \
             'decisions' is not accepted here — use record_decision, which is append-only.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; its directory is the project directory" },
                    "patch": {
                        "type": "object",
                        "description": "Partial document: any of 'functional_blocks', 'interfaces', 'net_priorities', each keyed by id"
                    }
                },
                "required": ["board", "patch"]
            }),
            |args, ctx| async move { handle_update_design_intent(args, ctx).await }
        ),
        tool!(
            "record_decision",
            "Append a timestamped engineering decision to the design-intent decision log. \
             Append-only — never edited or removed by set_design_intent / update_design_intent.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; its directory is the project directory" },
                    "decision": { "type": "string", "description": "What was decided, in plain English" },
                    "reason": { "type": "string", "description": "Why (optional)" },
                    "scope": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Reference designators or net names this decision applies to (optional)"
                    }
                },
                "required": ["board", "decision"]
            }),
            |args, ctx| async move { handle_record_decision(args, ctx).await }
        ),
        tool!(
            "analyze_design",
            "Non-mutating. Propose a DRAFT functional_blocks breakdown for human/agent review — \
             this never writes design_intent.json itself, call update_design_intent with \
             whatever you keep. Primary derivation: one candidate block per hierarchical sheet \
             (get_sheet_hierarchy), sheet name as the label, that sheet's own placed components \
             (list_schematic_components) as membership — NOT recursive, a parent sheet's block \
             holds only its own directly-placed parts, not its children's. Fallback for a flat \
             (non-hierarchical) schematic: if 'board' is given, cluster components by shared PCB \
             nets (union-find, same technique as auto_place_from_schematic); with no board, \
             returns a single block holding every component and says so. Also seeds \
             net_priorities defaults ('high') for recognized power/ground nets and matched \
             differential pairs (P/N, +/-, DP/DN suffixes) — a heuristic, not authoritative; \
             everything returned is a draft for review, never overclaimed as accurate.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Root .kicad_sch to start from" },
                    "board": { "type": "string", "description": "Optional .kicad_pcb — enables net-cluster fallback and PCB-based net_priorities seeding for flat schematics" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_analyze_design(args, ctx).await }
        ),
    ]
}

// ─── Schema constants ─────────────────────────────────────────────────────────

pub(crate) const PRIORITIES: &[&str] = &["critical", "high", "medium", "low"];
const INTERFACE_KINDS: &[&str] = &["high_speed", "differential", "analog", "power", "generic"];

// ─── Persistence — mirrors tools/config.rs's project.json convention exactly ──

fn default_design_intent() -> Value {
    json!({
        "version": 1,
        "functional_blocks": {},
        "interfaces": {},
        "net_priorities": {},
        "decisions": []
    })
}

fn design_intent_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".konnect").join("design_intent.json")
}

/// The project directory a "board" argument implies: its parent directory,
/// where `.konnect/` lives alongside the sibling `.kicad_pro` (same
/// convention `verification::sibling_project_path` and friends use). The
/// board file itself need not exist — design intent may be recorded before a
/// PCB does.
pub(crate) fn project_dir_from_board(board: &Path) -> PathBuf {
    board
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

async fn read_intent(path: &Path) -> Value {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| default_design_intent()),
        Err(_) => default_design_intent(),
    }
}

async fn write_intent(path: &Path, intent: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let content = serde_json::to_string_pretty(intent)?;
    tokio::fs::write(path, content).await?;
    Ok(())
}

// ─── Shape validation ─────────────────────────────────────────────────────────

fn invalid_intent(field: &str, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.clone(),
        },
        format!("Invalid design intent field '{field}': {reason}"),
    )
}

fn validate_functional_block(id: &str, block: &Value) -> Result<(), CallToolResult> {
    let obj = block
        .as_object()
        .ok_or_else(|| invalid_intent(&format!("functional_blocks.{id}"), "must be an object"))?;
    if let Some(priority) = obj.get("priority") {
        let priority = priority.as_str().ok_or_else(|| {
            invalid_intent(
                &format!("functional_blocks.{id}.priority"),
                "must be a string",
            )
        })?;
        if !PRIORITIES.contains(&priority) {
            return Err(invalid_intent(
                &format!("functional_blocks.{id}.priority"),
                format!("must be one of {PRIORITIES:?}, got '{priority}'"),
            ));
        }
    }
    if let Some(components) = obj.get("components") {
        let components = components.as_array().ok_or_else(|| {
            invalid_intent(
                &format!("functional_blocks.{id}.components"),
                "must be an array",
            )
        })?;
        if !components.iter().all(Value::is_string) {
            return Err(invalid_intent(
                &format!("functional_blocks.{id}.components"),
                "every entry must be a string reference designator",
            ));
        }
    }
    for text_field in ["label", "notes"] {
        if let Some(v) = obj.get(text_field) {
            if !v.is_string() {
                return Err(invalid_intent(
                    &format!("functional_blocks.{id}.{text_field}"),
                    "must be a string",
                ));
            }
        }
    }
    Ok(())
}

fn validate_interface(id: &str, iface: &Value) -> Result<(), CallToolResult> {
    let obj = iface
        .as_object()
        .ok_or_else(|| invalid_intent(&format!("interfaces.{id}"), "must be an object"))?;
    if let Some(kind) = obj.get("kind") {
        let kind = kind
            .as_str()
            .ok_or_else(|| invalid_intent(&format!("interfaces.{id}.kind"), "must be a string"))?;
        if !INTERFACE_KINDS.contains(&kind) {
            return Err(invalid_intent(
                &format!("interfaces.{id}.kind"),
                format!("must be one of {INTERFACE_KINDS:?}, got '{kind}'"),
            ));
        }
    }
    for array_field in ["blocks", "constraints"] {
        if let Some(v) = obj.get(array_field) {
            let arr = v.as_array().ok_or_else(|| {
                invalid_intent(
                    &format!("interfaces.{id}.{array_field}"),
                    "must be an array",
                )
            })?;
            if !arr.iter().all(Value::is_string) {
                return Err(invalid_intent(
                    &format!("interfaces.{id}.{array_field}"),
                    "every entry must be a string",
                ));
            }
        }
    }
    Ok(())
}

fn validate_net_priority(net: &str, priority: &Value) -> Result<(), CallToolResult> {
    let priority = priority.as_str().ok_or_else(|| {
        invalid_intent(
            &format!("net_priorities.{net}"),
            "priority must be a string",
        )
    })?;
    if !PRIORITIES.contains(&priority) {
        return Err(invalid_intent(
            &format!("net_priorities.{net}"),
            format!("must be one of {PRIORITIES:?}, got '{priority}'"),
        ));
    }
    Ok(())
}

fn validate_decision(index: usize, decision: &Value) -> Result<(), CallToolResult> {
    let obj = decision
        .as_object()
        .ok_or_else(|| invalid_intent(&format!("decisions[{index}]"), "must be an object"))?;
    if !obj.get("decision").is_some_and(Value::is_string) {
        return Err(invalid_intent(
            &format!("decisions[{index}].decision"),
            "must be a string",
        ));
    }
    Ok(())
}

fn validate_intent_shape(intent: &Value) -> Result<(), CallToolResult> {
    let obj = intent
        .as_object()
        .ok_or_else(|| invalid_intent("intent", "must be a JSON object"))?;

    if let Some(blocks) = obj.get("functional_blocks") {
        let blocks = blocks.as_object().ok_or_else(|| {
            invalid_intent("functional_blocks", "must be an object keyed by block_id")
        })?;
        for (id, block) in blocks {
            validate_functional_block(id, block)?;
        }
    }
    if let Some(interfaces) = obj.get("interfaces") {
        let interfaces = interfaces.as_object().ok_or_else(|| {
            invalid_intent("interfaces", "must be an object keyed by interface_id")
        })?;
        for (id, iface) in interfaces {
            validate_interface(id, iface)?;
        }
    }
    if let Some(priorities) = obj.get("net_priorities") {
        let priorities = priorities.as_object().ok_or_else(|| {
            invalid_intent("net_priorities", "must be an object keyed by net name")
        })?;
        for (net, priority) in priorities {
            validate_net_priority(net, priority)?;
        }
    }
    if let Some(decisions) = obj.get("decisions") {
        let decisions = decisions
            .as_array()
            .ok_or_else(|| invalid_intent("decisions", "must be an array"))?;
        for (i, d) in decisions.iter().enumerate() {
            validate_decision(i, d)?;
        }
    }
    Ok(())
}

/// Ensure `obj[field]` is an object (replacing a missing or wrong-typed value
/// with `{}`), then return a mutable handle to it. Never panics: a corrupted
/// on-disk document with e.g. `"functional_blocks": null` self-heals to `{}`
/// on the next update rather than crashing the handler.
fn ensure_object_field<'a>(
    obj: &'a mut Map<String, Value>,
    field: &str,
) -> &'a mut Map<String, Value> {
    if !obj.get(field).is_some_and(Value::is_object) {
        obj.insert(field.to_string(), json!({}));
    }
    obj.get_mut(field)
        .and_then(Value::as_object_mut)
        .expect("just inserted or already verified as an object")
}

/// Same self-healing guarantee as [`ensure_object_field`], for `decisions`.
fn ensure_array_field<'a>(obj: &'a mut Map<String, Value>, field: &str) -> &'a mut Vec<Value> {
    if !obj.get(field).is_some_and(Value::is_array) {
        obj.insert(field.to_string(), json!([]));
    }
    obj.get_mut(field)
        .and_then(Value::as_array_mut)
        .expect("just inserted or already verified as an array")
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_get_design_intent(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let project_dir = project_dir_from_board(&board);
    let path = design_intent_path(&project_dir);
    let exists = path.exists();
    let intent = read_intent(&path).await;

    Ok(CallToolResult::json(&json!({
        "design_intent": intent,
        "path": path.to_str().unwrap_or(""),
        "exists": exists
    })))
}

async fn handle_set_design_intent(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let intent_arg = &args["intent"];
    if intent_arg.is_null() {
        return Ok(CallToolResult::error("Missing required argument: 'intent'"));
    }
    let Some(overlay) = intent_arg.as_object() else {
        return Ok(invalid_intent("intent", "must be a JSON object"));
    };

    // Full replace, but missing top-level keys still default sensibly rather
    // than vanishing — an intent with only 'functional_blocks' set is a
    // coherent "I don't have interfaces/net_priorities/decisions yet" state,
    // not a caller error.
    let mut normalized = default_design_intent();
    let base = normalized
        .as_object_mut()
        .expect("default_design_intent() is always an object");
    for (key, value) in overlay {
        base.insert(key.clone(), value.clone());
    }
    base.insert("version".to_string(), json!(1));

    if let Err(error) = validate_intent_shape(&normalized) {
        return Ok(error);
    }

    let project_dir = project_dir_from_board(&board);
    let path = design_intent_path(&project_dir);
    write_intent(&path, &normalized).await?;

    Ok(CallToolResult::json(&json!({
        "written": true,
        "path": path.to_str().unwrap_or(""),
        "design_intent": normalized
    })))
}

async fn handle_update_design_intent(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let patch_arg = &args["patch"];
    if patch_arg.is_null() {
        return Ok(CallToolResult::error("Missing required argument: 'patch'"));
    }
    let Some(patch_obj) = patch_arg.as_object() else {
        return Ok(invalid_intent("patch", "must be a JSON object"));
    };

    const MERGEABLE_KEYS: &[&str] = &["functional_blocks", "interfaces", "net_priorities"];
    for key in patch_obj.keys() {
        if !MERGEABLE_KEYS.contains(&key.as_str()) {
            return Ok(invalid_intent(
                key,
                "update_design_intent only merges 'functional_blocks', 'interfaces', or \
                 'net_priorities' — use set_design_intent for a full replace, or \
                 record_decision to append a decision (the log is append-only)",
            ));
        }
    }

    // Validate every incoming entry BEFORE touching the on-disk document —
    // a half-applied patch (block A merged, block B rejected) would leave the
    // file in a state neither the caller nor a prior reader asked for.
    if let Some(blocks) = patch_obj
        .get("functional_blocks")
        .and_then(Value::as_object)
    {
        for (id, block) in blocks {
            if let Err(error) = validate_functional_block(id, block) {
                return Ok(error);
            }
        }
    }
    if let Some(interfaces) = patch_obj.get("interfaces").and_then(Value::as_object) {
        for (id, iface) in interfaces {
            if let Err(error) = validate_interface(id, iface) {
                return Ok(error);
            }
        }
    }
    if let Some(priorities) = patch_obj.get("net_priorities").and_then(Value::as_object) {
        for (net, priority) in priorities {
            if let Err(error) = validate_net_priority(net, priority) {
                return Ok(error);
            }
        }
    }

    let project_dir = project_dir_from_board(&board);
    let path = design_intent_path(&project_dir);
    let mut intent = read_intent(&path).await;
    let obj = intent
        .as_object_mut()
        .expect("read_intent always returns an object (default or parsed)");

    let mut updated_keys = Vec::new();
    for key in MERGEABLE_KEYS {
        if let Some(entries) = patch_obj.get(*key).and_then(Value::as_object) {
            let existing = ensure_object_field(obj, key);
            for (id, value) in entries {
                existing.insert(id.clone(), value.clone());
            }
            updated_keys.push(*key);
        }
    }

    write_intent(&path, &intent).await?;

    Ok(CallToolResult::json(&json!({
        "updated_keys": updated_keys,
        "design_intent": intent
    })))
}

async fn handle_record_decision(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let decision = match require_str(args, "decision") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let reason = opt_str(args, "reason").unwrap_or("").to_string();
    let scope: Vec<String> = match &args["scope"] {
        Value::Null => Vec::new(),
        Value::Array(items) => match items
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect()
        {
            Some(strings) => strings,
            None => return Ok(invalid_intent("scope", "every entry must be a string")),
        },
        _ => return Ok(invalid_intent("scope", "must be an array of strings")),
    };

    let project_dir = project_dir_from_board(&board);
    let path = design_intent_path(&project_dir);
    let mut intent = read_intent(&path).await;
    let obj = intent
        .as_object_mut()
        .expect("read_intent always returns an object (default or parsed)");

    let entry = json!({
        "timestamp": iso8601_now(),
        "decision": decision,
        "reason": reason,
        "scope": scope
    });
    let decisions = ensure_array_field(obj, "decisions");
    decisions.push(entry.clone());
    let total_decisions = decisions.len();

    write_intent(&path, &intent).await?;

    Ok(CallToolResult::json(&json!({
        "recorded": entry,
        "total_decisions": total_decisions
    })))
}

// ─── analyze_design ────────────────────────────────────────────────────────────

/// One candidate functional block: where it came from (a sheet file, or a
/// net cluster), its human label, and the reference designators found there.
struct BlockDraft {
    id: String,
    label: String,
    source_file: Option<String>,
    components: Vec<String>,
}

async fn handle_analyze_design(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let schematic = get_path(args, "schematic")?;
    if !schematic.exists() {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::FileNotFound {
                path: schematic.display().to_string(),
            },
            format!("Schematic '{}' not found", schematic.display()),
        ));
    }
    let board = opt_str(args, "board").map(PathBuf::from);

    let project_name = crate::tools::project_name_for(&schematic);
    let mut visited = HashSet::new();
    let tree =
        super::sch_hierarchy::build_hierarchy_node(&schematic, &project_name, 0, &mut visited)?;

    let root_dir = schematic
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut used_ids = HashSet::new();
    let root_id = unique_sanitized_id(&project_name, &mut used_ids);
    let mut sheet_files: Vec<(String, String, PathBuf)> = Vec::new();
    sheet_files.push((root_id, project_name.clone(), schematic.clone()));
    collect_sheet_nodes(&tree, &root_dir, &mut used_ids, &mut sheet_files);

    let hierarchical = sheet_files.len() > 1;
    let mut blocks: Vec<BlockDraft> = Vec::new();

    if hierarchical {
        for (id, label, file) in &sheet_files {
            let components = components_in_schematic(file, ctx).await?;
            blocks.push(BlockDraft {
                id: id.clone(),
                label: label.clone(),
                source_file: Some(file.display().to_string()),
                components,
            });
        }
    } else {
        // Flat schematic: no child sheets to key blocks off of.
        let (_, _, root_file) = &sheet_files[0];
        let root_components = components_in_schematic(root_file, ctx).await?;

        if let Some(board_path) = board.as_deref().filter(|b| b.exists()) {
            for (cluster_index, members) in cluster_by_shared_nets(board_path, &root_components)?
                .into_iter()
                .enumerate()
            {
                blocks.push(BlockDraft {
                    id: format!("cluster_{}", cluster_index + 1),
                    label: format!("Net cluster {}", cluster_index + 1),
                    source_file: None,
                    components: members,
                });
            }
        } else {
            blocks.push(BlockDraft {
                id: "all_components".to_string(),
                label: project_name.clone(),
                source_file: None,
                components: root_components,
            });
        }
    }

    // net_priorities draft: PCB nets if a board was given and reads cleanly,
    // otherwise net labels collected from every discovered schematic file.
    let net_names: BTreeSet<String> = match board.as_deref().filter(|b| b.exists()) {
        Some(board_path) => nets_from_board(board_path).unwrap_or_default(),
        None => {
            let files: Vec<&Path> = sheet_files.iter().map(|(_, _, f)| f.as_path()).collect();
            nets_from_schematics(&files)
        }
    };
    let naming = effective_naming_conventions(
        board
            .as_deref()
            .map(project_dir_from_board)
            .or_else(|| schematic.parent().map(Path::to_path_buf)),
        ctx,
    )
    .await;
    let power_prefix = naming["net_prefix_power"].as_str().unwrap_or("VCC_");
    let ground_prefix = naming["net_prefix_ground"].as_str().unwrap_or("GND");
    let net_priorities = classify_net_priorities(&net_names, power_prefix, ground_prefix);

    let derivation = if hierarchical {
        "hierarchical_sheets"
    } else if board.as_deref().is_some_and(Path::exists) {
        "pcb_net_clusters"
    } else {
        "single_block_no_hierarchy_no_board"
    };

    let functional_blocks: Map<String, Value> = blocks
        .iter()
        .map(|b| {
            let notes = match &b.source_file {
                Some(file) => format!(
                    "Heuristic draft derived from hierarchical sheet '{file}'. Not authoritative \
                     — review membership and priority before calling update_design_intent."
                ),
                None if derivation == "pcb_net_clusters" => {
                    "Heuristic draft: components sharing at least one PCB net, grouped by \
                     union-find (same technique as auto_place_from_schematic). No hierarchical \
                     sheets were found, so this is a connectivity guess, not an engineering \
                     grouping — review before committing."
                        .to_string()
                }
                None => format!(
                    "No hierarchical sheets and no 'board' argument, so every component in \
                     '{}' landed in one block — this draft cannot subdivide further. Add \
                     hierarchical sheets or pass 'board' for a real breakdown.",
                    project_name
                ),
            };
            (
                b.id.clone(),
                json!({
                    "label": b.label,
                    "priority": "medium",
                    "components": b.components,
                    "notes": notes
                }),
            )
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "draft": true,
        "note": "Heuristic proposal for human/agent review, not authoritative. Nothing was \
                 written — call update_design_intent with whatever you keep.",
        "derivation": derivation,
        "sheet_count": sheet_files.len(),
        "functional_blocks": functional_blocks,
        "net_priorities": net_priorities
    })))
}

/// Depth-first walk of a `build_hierarchy_node` tree, resolving each child's
/// file against ITS OWN containing directory (the parent node's file's
/// directory) rather than assuming every sheet lives beside the root — a
/// child sheet can be filed in a subdirectory of its parent.
fn collect_sheet_nodes(
    node: &Value,
    dir: &Path,
    used_ids: &mut HashSet<String>,
    out: &mut Vec<(String, String, PathBuf)>,
) {
    let Some(children) = node["children"].as_array() else {
        return;
    };
    for child in children {
        // A broken reference (missing file, cycle, parse failure) has nothing
        // to derive membership from — skip it rather than guessing.
        if child.get("error").is_some() {
            continue;
        }
        let (Some(name), Some(file)) = (child["name"].as_str(), child["file"].as_str()) else {
            continue;
        };
        let child_path = dir.join(file);
        if !child_path.exists() {
            continue;
        }
        let id = unique_sanitized_id(name, used_ids);
        let child_dir = child_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.to_path_buf());
        out.push((id, name.to_string(), child_path.clone()));
        collect_sheet_nodes(child, &child_dir, used_ids, out);
    }
}

async fn components_in_schematic(file: &Path, ctx: &ToolContext) -> anyhow::Result<Vec<String>> {
    let result = super::sch_components::handle_list_schematic_components(
        &json!({ "schematic": file.to_string_lossy() }),
        ctx,
    )
    .await?;
    if result.is_error {
        // A sheet the hierarchy walk found but that fails to parse as a
        // component list contributes no membership rather than failing the
        // whole draft — analyze_design is best-effort by design.
        return Ok(Vec::new());
    }
    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
        return Ok(Vec::new());
    };
    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    Ok(body["components"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|c| c["reference"].as_str())
                .filter(|r| *r != "?")
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// Union-find clustering by shared PCB net, restricted to the given
/// reference set (the schematic's own components) — the same technique
/// `placement::handle_auto_place` uses for its first-placement clusters,
/// reimplemented here at schematic-block granularity since that pass isn't
/// factored into a reusable function.
fn cluster_by_shared_nets(board: &Path, references: &[String]) -> anyhow::Result<Vec<Vec<String>>> {
    let content = konnect_sexp::writer::read_consistent(board)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let index = PcbConnectivityIndex::build(&tree);

    let mut ordered: Vec<String> = references.to_vec();
    ordered.sort();
    ordered.dedup();
    let ref_index: BTreeMap<&str, usize> = ordered
        .iter()
        .enumerate()
        .map(|(i, r)| (r.as_str(), i))
        .collect();

    let mut parent: Vec<usize> = (0..ordered.len()).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        if parent[i] != i {
            let root = find(parent, parent[i]);
            parent[i] = root;
        }
        parent[i]
    }
    for net in index.nets() {
        let mut prev: Option<usize> = None;
        for pad in index.pads_of_net(net) {
            if let Some(&i) = ref_index.get(pad.reference.as_str()) {
                if let Some(p) = prev {
                    let (a, b) = (find(&mut parent, p), find(&mut parent, i));
                    if a != b {
                        parent[a.max(b)] = a.min(b);
                    }
                }
                prev = Some(i);
            }
        }
    }

    let mut clusters: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (i, reference) in ordered.iter().enumerate() {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(reference.clone());
    }
    Ok(clusters.into_values().collect())
}

fn nets_from_board(board: &Path) -> anyhow::Result<BTreeSet<String>> {
    let content = konnect_sexp::writer::read_consistent(board)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;
    let index = PcbConnectivityIndex::build(&tree);
    Ok(index.nets().into_iter().map(str::to_string).collect())
}

fn nets_from_schematics(files: &[&Path]) -> BTreeSet<String> {
    let mut nets = BTreeSet::new();
    for file in files {
        if let Ok((_, tree)) = konnect_sexp::schematic::read_schematic(file) {
            for label in konnect_sexp::schematic::extract_all_net_labels(&tree) {
                if !label.net.is_empty() {
                    nets.insert(label.net);
                }
            }
        }
    }
    nets
}

/// `naming_conventions` from `get_effective_config` (user defaults + project
/// overrides) — reused rather than re-derived, per this codebase's evidence
/// doctrine. Falls back to the same defaults `config::default_user_config`
/// declares if the lookup fails for any reason (e.g. no project directory).
async fn effective_naming_conventions(project_dir: Option<PathBuf>, ctx: &ToolContext) -> Value {
    let mut args = json!({});
    if let Some(dir) = project_dir {
        args["project_dir"] = json!(dir.to_string_lossy());
    }
    let fallback = json!({ "net_prefix_power": "VCC_", "net_prefix_ground": "GND" });
    let Ok(result) = super::config::handle_get_effective_config(&args, ctx).await else {
        return fallback;
    };
    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
        return fallback;
    };
    let Ok(body) = serde_json::from_str::<Value>(text) else {
        return fallback;
    };
    let naming = &body["effective_config"]["naming_conventions"];
    json!({
        "net_prefix_power": naming["net_prefix_power"].as_str().unwrap_or("VCC_"),
        "net_prefix_ground": naming["net_prefix_ground"].as_str().unwrap_or("GND")
    })
}

/// Seed `net_priorities` defaults: power/ground nets and matched differential
/// pairs default to `"high"`. Everything else is left unset — this is a
/// starting point for review, not a claim that every other net is
/// unimportant.
fn classify_net_priorities(
    nets: &BTreeSet<String>,
    power_prefix: &str,
    ground_prefix: &str,
) -> BTreeMap<String, &'static str> {
    let mut out = BTreeMap::new();
    let ground_upper = ground_prefix.to_ascii_uppercase();

    for net in nets {
        let upper = net.to_ascii_uppercase();
        let is_ground =
            !ground_upper.is_empty() && (upper == ground_upper || upper.starts_with(&ground_upper));
        let is_power = !power_prefix.is_empty() && net.starts_with(power_prefix);
        if is_ground || is_power {
            out.insert(net.clone(), "high");
        }
    }

    const DIFF_PAIR_SUFFIXES: &[(&str, &str)] = &[("_P", "_N"), ("+", "-"), ("_DP", "_DN")];
    for (positive, negative) in DIFF_PAIR_SUFFIXES {
        for net in nets {
            if let Some(base) = net.strip_suffix(positive) {
                let partner = format!("{base}{negative}");
                if nets.contains(&partner) {
                    out.insert(net.clone(), "high");
                    out.insert(partner, "high");
                }
            }
        }
    }
    out
}

fn sanitize_id(name: &str) -> String {
    let mut out = String::new();
    let mut last_was_sep = true; // suppress a leading separator
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }
    let trimmed = out.trim_end_matches('_');
    if trimmed.is_empty() {
        "block".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `sanitize_id`, disambiguated against ids already handed out this walk —
/// two sheets named e.g. "Power" and "Power!" would otherwise collide on the
/// same sanitized id and silently overwrite one another's draft.
fn unique_sanitized_id(name: &str, used: &mut HashSet<String>) -> String {
    let base = sanitize_id(name);
    if used.insert(base.clone()) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}_{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn board_path(tmp: &TempDir) -> PathBuf {
        // design_intent operations never require the .kicad_pcb to exist —
        // only its parent directory, which IS the project directory.
        tmp.path().join("board.kicad_pcb")
    }

    fn result_json(result: &CallToolResult) -> Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    // ── get / set / update round trip ──────────────────────────────────────

    #[tokio::test]
    async fn get_design_intent_defaults_to_empty_skeleton_when_no_file_exists() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_path(&tmp);

        let result = handle_get_design_intent(&json!({ "board": board.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["exists"], false);
        assert_eq!(body["design_intent"]["version"], 1);
        assert_eq!(body["design_intent"]["functional_blocks"], json!({}));
        assert_eq!(body["design_intent"]["decisions"], json!([]));
    }

    #[tokio::test]
    async fn set_then_get_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_path(&tmp);

        let intent = json!({
            "functional_blocks": {
                "power": { "label": "Power Supply", "priority": "high", "components": ["U1", "C1"] }
            },
            "net_priorities": { "VCC_5V0": "critical" }
        });
        let set_result = handle_set_design_intent(
            &json!({ "board": board.to_string_lossy(), "intent": intent }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!set_result.is_error, "{set_result:?}");

        // Design intent lives beside the (not-yet-existing) board, at
        // <project_dir>/.konnect/design_intent.json — same convention as
        // project.json.
        assert!(tmp
            .path()
            .join(".konnect")
            .join("design_intent.json")
            .exists());

        let get_result =
            handle_get_design_intent(&json!({ "board": board.to_string_lossy() }), &ctx)
                .await
                .unwrap();
        let body = result_json(&get_result);
        assert_eq!(body["exists"], true);
        assert_eq!(
            body["design_intent"]["functional_blocks"]["power"]["label"],
            "Power Supply"
        );
        assert_eq!(
            body["design_intent"]["net_priorities"]["VCC_5V0"],
            "critical"
        );
        // Untouched top-level keys still default rather than vanishing.
        assert_eq!(body["design_intent"]["interfaces"], json!({}));
    }

    #[tokio::test]
    async fn set_design_intent_rejects_an_invalid_priority() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_path(&tmp);

        let intent = json!({
            "functional_blocks": { "power": { "priority": "urgent" } }
        });
        let result = handle_set_design_intent(
            &json!({ "board": board.to_string_lossy(), "intent": intent }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let body = result_json(&result);
        assert_eq!(body["error"]["kind"], "invalid_argument");
        assert!(body["error"]["field"]
            .as_str()
            .unwrap()
            .contains("priority"));
    }

    #[tokio::test]
    async fn update_design_intent_merges_one_block_without_touching_others() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_path(&tmp);

        handle_set_design_intent(
            &json!({
                "board": board.to_string_lossy(),
                "intent": { "functional_blocks": {
                    "power": { "label": "Power", "components": ["U1"] },
                    "mcu": { "label": "MCU", "components": ["U2"] }
                }}
            }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_update_design_intent(
            &json!({
                "board": board.to_string_lossy(),
                "patch": { "functional_blocks": {
                    "power": { "label": "Power Supply", "components": ["U1", "C1"] }
                }}
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(
            body["design_intent"]["functional_blocks"]["power"]["label"],
            "Power Supply"
        );
        // 'mcu' was never in the patch — it must survive untouched.
        assert_eq!(
            body["design_intent"]["functional_blocks"]["mcu"]["label"],
            "MCU"
        );
    }

    #[tokio::test]
    async fn update_design_intent_refuses_a_decisions_key() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_path(&tmp);

        let result = handle_update_design_intent(
            &json!({
                "board": board.to_string_lossy(),
                "patch": { "decisions": [{ "decision": "sneaky" }] }
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let body = result_json(&result);
        assert_eq!(body["error"]["kind"], "invalid_argument");
    }

    // ── decision log ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn record_decision_appends_and_is_readable_back() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_path(&tmp);

        let first = handle_record_decision(
            &json!({
                "board": board.to_string_lossy(),
                "decision": "Route USB D+/D- as 90ohm diff pair",
                "reason": "USB2 spec",
                "scope": ["USB_DP", "USB_DN"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!first.is_error, "{first:?}");
        let first_body = result_json(&first);
        assert_eq!(first_body["total_decisions"], 1);
        assert!(first_body["recorded"]["timestamp"]
            .as_str()
            .unwrap()
            .ends_with('Z'));

        let second = handle_record_decision(
            &json!({ "board": board.to_string_lossy(), "decision": "Second decision" }),
            &ctx,
        )
        .await
        .unwrap();
        let second_body = result_json(&second);
        assert_eq!(second_body["total_decisions"], 2);
        // reason/scope default rather than erroring when omitted.
        assert_eq!(second_body["recorded"]["reason"], "");
        assert_eq!(second_body["recorded"]["scope"], json!([]));

        let get_result =
            handle_get_design_intent(&json!({ "board": board.to_string_lossy() }), &ctx)
                .await
                .unwrap();
        let body = result_json(&get_result);
        let decisions = body["design_intent"]["decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(
            decisions[0]["decision"],
            "Route USB D+/D- as 90ohm diff pair"
        );
        assert_eq!(decisions[0]["scope"], json!(["USB_DP", "USB_DN"]));
    }

    // ── analyze_design ──────────────────────────────────────────────────────

    fn write_blank_schematic(path: &Path) {
        let template = crate::tools::blank_schematic_template();
        konnect_sexp::writer::write_new_atomic(path, &template).unwrap();
    }

    /// A minimal .kicad_sch with two symbols placed directly (raw
    /// S-expression text — the pattern `sch_analysis.rs`'s own tests use to
    /// avoid needing a real symbol library), so `list_schematic_components`
    /// has references to find without going through `add_schematic_component`
    /// (which needs a resolvable library symbol).
    fn schematic_with_symbols(path: &Path, refs: &[(&str, &str)]) {
        let symbols: String = refs
            .iter()
            .enumerate()
            .map(|(i, (reference, value))| {
                format!(
                    "  (symbol (lib_id \"Device:R\") (at {x} 100 0) (unit 1) (uuid \"u{i}\")\n    \
                     (property \"Reference\" \"{reference}\" (at {x} 98 0))\n    \
                     (property \"Value\" \"{value}\" (at {x} 102 0))\n  )\n",
                    x = 100 + i as i32 * 20
                )
            })
            .collect();
        let content = format!(
            "(kicad_sch\n  (version 20260306)\n  (generator \"eeschema\")\n  (uuid \"{}\")\n{}  \
             (sheet_instances (path \"/\" (page \"1\")))\n)\n",
            konnect_sexp::writer::new_uuid(),
            symbols
        );
        std::fs::write(path, content).unwrap();
    }

    fn add_child_sheet(root: &Path, dir: &Path, name: &str, file: &str) {
        let spec = konnect_sexp::schematic::HierarchicalSheetSpec {
            name,
            file,
            x: 20.0,
            y: 20.0,
            width: 40.0,
            height: 30.0,
            project_name: &crate::tools::project_name_for(root),
            parent_instance_path: "/",
            page: "2",
        };
        let block = konnect_sexp::schematic::format_hierarchical_sheet(spec);
        let content = std::fs::read_to_string(root).unwrap();
        // Splice the sheet block in as one more child of `(kicad_sch ...)`,
        // right before its closing paren — works regardless of whether the
        // file has a top-level `sheet_instances` block (a blank schematic
        // from `blank_schematic_template()` doesn't).
        let insert_at = content
            .rfind(')')
            .expect("schematic must end with a closing paren");
        let mut updated = String::with_capacity(content.len() + block.len() + 1);
        updated.push_str(&content[..insert_at]);
        updated.push_str(&block);
        updated.push('\n');
        updated.push_str(&content[insert_at..]);
        std::fs::write(root, updated).unwrap();
        schematic_with_symbols(&dir.join(file), &[]);
    }

    #[tokio::test]
    async fn analyze_design_produces_one_block_per_hierarchical_sheet() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = tmp.path().join("root.kicad_sch");
        write_blank_schematic(&root);
        add_child_sheet(&root, tmp.path(), "Power", "power.kicad_sch");
        add_child_sheet(&root, tmp.path(), "Camera", "camera.kicad_sch");
        schematic_with_symbols(&tmp.path().join("power.kicad_sch"), &[("U1", "AP2112")]);
        schematic_with_symbols(
            &tmp.path().join("camera.kicad_sch"),
            &[("J1", "Camera_Conn"), ("C1", "100nF")],
        );

        let result = handle_analyze_design(&json!({ "schematic": root.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["derivation"], "hierarchical_sheets");
        assert_eq!(body["draft"], true);

        let blocks = body["functional_blocks"].as_object().unwrap();
        // root (blank, no components) + Power + Camera = 3 candidate blocks.
        assert_eq!(blocks.len(), 3, "{blocks:?}");

        let power = blocks
            .values()
            .find(|b| b["label"] == "Power")
            .expect("a Power block");
        assert_eq!(power["components"], json!(["U1"]));

        let camera = blocks
            .values()
            .find(|b| b["label"] == "Camera")
            .expect("a Camera block");
        let camera_components: BTreeSet<String> = camera["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            camera_components,
            BTreeSet::from(["J1".to_string(), "C1".to_string()])
        );
    }

    #[tokio::test]
    async fn analyze_design_with_no_hierarchy_and_no_board_returns_one_block() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let root = tmp.path().join("flat.kicad_sch");
        schematic_with_symbols(&root, &[("R1", "10k"), ("R2", "10k")]);

        let result = handle_analyze_design(&json!({ "schematic": root.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["derivation"], "single_block_no_hierarchy_no_board");
        let blocks = body["functional_blocks"].as_object().unwrap();
        assert_eq!(blocks.len(), 1);
        let only = blocks.values().next().unwrap();
        let components: BTreeSet<String> = only["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            components,
            BTreeSet::from(["R1".to_string(), "R2".to_string()])
        );
    }

    #[tokio::test]
    async fn analyze_design_reports_file_not_found_for_a_missing_schematic() {
        let ctx = test_ctx();
        let result =
            handle_analyze_design(&json!({ "schematic": "/nonexistent/nope.kicad_sch" }), &ctx)
                .await
                .unwrap();
        assert!(result.is_error);
        let body = result_json(&result);
        assert_eq!(body["error"]["kind"], "file_not_found");
    }

    // ── classify_net_priorities ─────────────────────────────────────────────

    #[test]
    fn classify_net_priorities_seeds_power_ground_and_diff_pairs_as_high() {
        let nets: BTreeSet<String> = [
            "VCC_3V3",
            "GND",
            "USB_D_P",
            "USB_D_N",
            "SPI_CLK",
            "GND_ANALOG",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let priorities = classify_net_priorities(&nets, "VCC_", "GND");

        assert_eq!(priorities.get("VCC_3V3"), Some(&"high"));
        assert_eq!(priorities.get("GND"), Some(&"high"));
        assert_eq!(priorities.get("GND_ANALOG"), Some(&"high"));
        assert_eq!(priorities.get("USB_D_P"), Some(&"high"));
        assert_eq!(priorities.get("USB_D_N"), Some(&"high"));
        // Not power, not ground, no matched partner — left for human review.
        assert_eq!(priorities.get("SPI_CLK"), None);
    }

    #[test]
    fn sanitize_id_disambiguates_collisions() {
        let mut used = HashSet::new();
        assert_eq!(
            unique_sanitized_id("Power Supply", &mut used),
            "power_supply"
        );
        assert_eq!(
            unique_sanitized_id("Power Supply!", &mut used),
            "power_supply_2"
        );
    }
}
