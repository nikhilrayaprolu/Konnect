//! `phase_gate` toolset — phase-gated workflow enforcement for PCB routing.
//!
//! Ad-hoc tool calls let an agent route a board before it has ever scored a
//! placement, or before a single functional block has been named — nothing
//! in the tool surface stops "call route_trace 200 times" from being the
//! very first thing that happens to a fresh board. This toolset gives the
//! workflow an explicit state machine so that class of mistake becomes a
//! structured refusal instead of a routing mess:
//!
//! ```text
//! analysis → floorplan → placement → critical_routing → routing → planes → verification
//! ```
//!
//! State is persisted at `<project_dir>/.konnect/phase_state.json`, using the
//! exact same read/write convention `tools/config.rs` uses for
//! `project.json` (see `tools/design_intent.rs`'s module doc for the shared
//! rationale — this toolset's persistence code mirrors it byte for byte).
//!
//! ## Phase order: skip-ahead is allowed, going back is always free
//!
//! `advance_phase` does not require the caller to visit every intermediate
//! phase — jumping straight from `analysis` to `critical_routing` is
//! allowed, subject only to `critical_routing`'s OWN gate criteria (it does
//! NOT retroactively check `floorplan`'s or `placement`'s criteria for the
//! phases skipped over). This is a deliberate simplification: each phase's
//! criteria describe what must be true to safely ENTER it, not a checklist
//! that accumulates. A caller who skips `placement` straight into
//! `critical_routing` still has to clear `critical_routing`'s own bar
//! (`score_placement` must not hard-fail), which in practice makes skipping
//! ahead self-defeating for phases whose criteria depend on the skipped
//! work having happened — but the gate does not pretend to model that
//! dependency chain explicitly.
//!
//! Moving to an EARLIER or the SAME phase never checks criteria and never
//! needs `force` — going back to redo work cannot skip a step by
//! definition. Only a forward move gates.
//!
//! ## The `force` escape hatch
//!
//! `advance_phase(force: true, reason: "...")` always succeeds (skipping
//! criteria evaluation entirely) and always requires a non-empty `reason`,
//! which is appended to `overrides` in `phase_state.json` alongside a
//! timestamp. This is not a bypass to route around — a human engineer
//! legitimately needs to jump back and forth, and the point of `force` is
//! that the override is RECORDED, not that it is hard to reach.
//!
//! ## Enforcement surface: routing tools only, deliberately narrow
//!
//! [`require_phase_at_least`] is wired into exactly four tools in
//! `pcb_routing.rs`: `route_trace`, `route_pad_to_pad`, `add_via`,
//! `route_differential_pair` — every one of them gated at
//! `Phase::CriticalRouting`. Retrofitting phase checks onto the other 220+
//! tools in this server is explicitly out of scope for this change; see the
//! module's own doc comment there for the exact reasoning. Extending the
//! gate to more tools is a separate decision, not a mechanical follow-up.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, iso8601_now, opt_str, require_str, ToolContext, ToolDef};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use super::design_intent::project_dir_from_board;

// ─── Phase ─────────────────────────────────────────────────────────────────────

/// The seven workflow phases, in the order `advance_phase` compares them.
/// `Ord` follows declaration order, so `Phase::Routing > Phase::Placement`
/// etc. compile to the intended "later in the workflow" comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Analysis,
    Floorplan,
    Placement,
    CriticalRouting,
    Routing,
    Planes,
    Verification,
}

impl Phase {
    const ALL: [Phase; 7] = [
        Phase::Analysis,
        Phase::Floorplan,
        Phase::Placement,
        Phase::CriticalRouting,
        Phase::Routing,
        Phase::Planes,
        Phase::Verification,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Analysis => "analysis",
            Phase::Floorplan => "floorplan",
            Phase::Placement => "placement",
            Phase::CriticalRouting => "critical_routing",
            Phase::Routing => "routing",
            Phase::Planes => "planes",
            Phase::Verification => "verification",
        }
    }

    pub fn from_name(name: &str) -> Option<Phase> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }

    fn all_names() -> Vec<&'static str> {
        Self::ALL.iter().map(|p| p.as_str()).collect()
    }
}

// ─── Persistence — mirrors tools/config.rs's project.json convention exactly ──

fn default_phase_state() -> Value {
    let now = iso8601_now();
    json!({
        "current_phase": Phase::Analysis.as_str(),
        "completed_phases": [],
        "phase_history": [
            { "phase": Phase::Analysis.as_str(), "entered_at": now, "exited_at": Value::Null }
        ],
        "overrides": []
    })
}

fn phase_state_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".konnect").join("phase_state.json")
}

async fn read_state(path: &Path) -> Value {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| default_phase_state()),
        Err(_) => default_phase_state(),
    }
}

async fn write_state(path: &Path, state: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let content = serde_json::to_string_pretty(state)?;
    tokio::fs::write(path, content).await?;
    Ok(())
}

fn current_phase_of(state: &Value) -> Phase {
    state["current_phase"]
        .as_str()
        .and_then(Phase::from_name)
        .unwrap_or(Phase::Analysis)
}

fn ensure_array_field<'a>(obj: &'a mut Map<String, Value>, field: &str) -> &'a mut Vec<Value> {
    if !obj.get(field).is_some_and(Value::is_array) {
        obj.insert(field.to_string(), json!([]));
    }
    obj.get_mut(field)
        .and_then(Value::as_array_mut)
        .expect("just inserted or already verified as an array")
}

// ─── Gate criteria ─────────────────────────────────────────────────────────────

/// What has to be true to enter `target`. Returns each unmet criterion as a
/// human-readable string naming what failed and what to do about it — empty
/// means the gate is clear. Errors reading board/design-intent state are
/// themselves reported as unmet criteria (fail closed) rather than
/// propagated as a hard `Err`, so a transient read failure blocks a phase
/// advance instead of silently permitting one.
async fn unmet_criteria_for(target: Phase, board: &Path, ctx: &ToolContext) -> Vec<String> {
    match target {
        Phase::Placement => {
            let project_dir = project_dir_from_board(board);
            let intent_path = project_dir.join(".konnect").join("design_intent.json");
            let intent = match tokio::fs::read_to_string(&intent_path).await {
                Ok(content) => serde_json::from_str::<Value>(&content).unwrap_or(Value::Null),
                Err(_) => Value::Null,
            };
            let has_block = intent["functional_blocks"]
                .as_object()
                .is_some_and(|blocks| !blocks.is_empty());
            if has_block {
                Vec::new()
            } else {
                vec![
                    "design_intent.json has no functional_blocks defined — call analyze_design \
                     then update_design_intent (design_intent toolset), or add at least one \
                     manually via set_design_intent/update_design_intent"
                        .to_string(),
                ]
            }
        }
        Phase::CriticalRouting | Phase::Routing => {
            if !board.exists() {
                return vec![format!(
                    "board file '{}' does not exist — cannot score placement",
                    board.display()
                )];
            }
            let args = json!({ "board": board.to_string_lossy() });
            match super::placement::handle_score_placement(&args, ctx).await {
                Ok(result) if !result.is_error => {
                    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0]
                    else {
                        return vec!["score_placement returned non-text content".to_string()];
                    };
                    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
                    let verdict = body["verdict"].as_str().unwrap_or("");
                    if verdict == "hard_fail" {
                        vec![format!(
                            "score_placement verdict is 'hard_fail' (score {}) — same-side \
                             courtyard overlaps or parts outside the board outline must be fixed \
                             before routing; see hard_failures in score_placement's own response",
                            body["score"]
                        )]
                    } else {
                        Vec::new()
                    }
                }
                Ok(result) => {
                    vec![format!(
                        "score_placement could not be evaluated: {}",
                        result.content.first().map_or(String::new(), |c| match c {
                            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
                            _ => String::new(),
                        })
                    )]
                }
                Err(error) => vec![format!("score_placement failed to run: {error}")],
            }
        }
        Phase::Verification => {
            if !board.exists() {
                return vec![format!(
                    "board file '{}' does not exist — cannot check routing completeness",
                    board.display()
                )];
            }
            // Threshold: zero. Entering 'verification' is the caller's claim
            // that routing is DONE; DRC's unconnected_items category reports
            // exactly the missing-connection defects that claim would
            // contradict. A `null` (category not reported by this kicad-cli)
            // is treated the same as a nonzero count — fail closed, since
            // "never asked" and "zero found" must not look the same here.
            let args =
                json!({ "board": board.to_string_lossy(), "severity": "warning", "limit": 1 });
            match super::verification::handle_run_drc(&args, ctx).await {
                Ok(result) if !result.is_error => {
                    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0]
                    else {
                        return vec!["run_drc returned non-text content".to_string()];
                    };
                    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
                    match body["unconnected_items"].as_u64() {
                        Some(0) => Vec::new(),
                        Some(n) => vec![format!(
                            "run_drc reports {n} unconnected_items — routing is not complete"
                        )],
                        None => vec![
                            "run_drc did not report the unconnected_items category — routing \
                             completeness cannot be confirmed"
                                .to_string(),
                        ],
                    }
                }
                Ok(_) => vec!["run_drc could not be evaluated".to_string()],
                Err(error) => vec![format!(
                    "run_drc failed to run: {error} — cannot confirm routing completeness"
                )],
            }
        }
        // No additional criteria defined yet for these phases — reachable by
        // phase order (and the target-name/force checks in advance_phase)
        // alone.
        Phase::Analysis | Phase::Floorplan | Phase::Planes => Vec::new(),
    }
}

/// Refuse a routing mutation unless the project's persisted phase is at
/// least `min_phase`. Reusable gate for `pcb_routing.rs`'s mutation tools —
/// see the module doc for exactly which ones and why only those.
///
/// Reads `phase_state.json` fresh on every call rather than caching: the
/// state is small, changes rarely mid-session, and a stale in-memory phase
/// letting a routing call through after the project was deliberately walked
/// back a phase is a worse failure mode than one extra file read per call.
pub(crate) async fn require_phase_at_least(
    board: &Path,
    min_phase: Phase,
) -> Result<(), CallToolResult> {
    let project_dir = project_dir_from_board(board);
    let path = phase_state_path(&project_dir);
    let state = read_state(&path).await;
    let current = current_phase_of(&state);

    if current >= min_phase {
        return Ok(());
    }

    let unmet = vec![format!(
        "project phase is '{}', but this tool requires at least '{}'",
        current.as_str(),
        min_phase.as_str()
    )];
    Err(CallToolResult::error_kind(
        ToolErrorKind::PhaseGateBlocked {
            current_phase: current.as_str().to_string(),
            required_phase: min_phase.as_str().to_string(),
            unmet_criteria: unmet,
        },
        format!(
            "Routing is phase-gated: project phase is '{}', this tool requires at least '{}'. \
             Call advance_phase(board, target_phase: \"{}\") once its gate criteria are met \
             (phase_gate toolset), or with force: true and a reason if this is a deliberate \
             out-of-order edit.",
            current.as_str(),
            min_phase.as_str(),
            min_phase.as_str()
        ),
    ))
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "get_phase_state",
            "Read the project's persisted workflow phase (analysis → floorplan → placement → \
             critical_routing → routing → planes → verification), its history, and any \
             recorded force overrides, from <project_dir>/.konnect/phase_state.json. Defaults \
             to phase 'analysis' — not an error — when no file exists yet.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; its directory is the project directory" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_phase_state(args, ctx).await }
        ),
        tool!(
            "advance_phase",
            "Move the project to a new workflow phase. Moving to a LATER phase checks that \
             phase's own gate criteria first (entering 'placement' needs at least one \
             functional_block in design_intent.json; 'critical_routing'/'routing' need \
             score_placement to not report 'hard_fail'; 'verification' needs run_drc's \
             unconnected_items at zero) and refuses naming exactly what's unmet if they \
             aren't met. Moving to an EARLIER or the same phase never gates. \
             'force: true' skips criteria entirely but requires a non-empty 'reason', which is \
             appended to phase_state.json's 'overrides' log — the deliberate escape hatch for \
             legitimate out-of-order work, not a bypass to avoid.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; its directory is the project directory" },
                    "target_phase": {
                        "type": "string",
                        "enum": ["analysis", "floorplan", "placement", "critical_routing", "routing", "planes", "verification"]
                    },
                    "force": { "type": "boolean", "default": false, "description": "Skip gate criteria. Requires 'reason'." },
                    "reason": { "type": "string", "description": "Required when force is true; recorded in phase_state.json's overrides" }
                },
                "required": ["board", "target_phase"]
            }),
            |args, ctx| async move { handle_advance_phase(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_get_phase_state(
    args: &Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let project_dir = project_dir_from_board(&board);
    let path = phase_state_path(&project_dir);
    let exists = path.exists();
    let state = read_state(&path).await;

    Ok(CallToolResult::json(&json!({
        "phase_state": state,
        "path": path.to_str().unwrap_or(""),
        "exists": exists
    })))
}

async fn handle_advance_phase(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let target_name = match require_str(args, "target_phase") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let Some(target) = Phase::from_name(target_name) else {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "target_phase".to_string(),
                reason: format!(
                    "must be one of {:?}, got '{target_name}'",
                    Phase::all_names()
                ),
            },
            format!("Unknown phase '{target_name}'"),
        ));
    };
    let force = args["force"].as_bool().unwrap_or(false);
    let reason = opt_str(args, "reason")
        .map(str::trim)
        .filter(|r| !r.is_empty());

    if force && reason.is_none() {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument {
                field: "reason".to_string(),
                reason: "force: true requires a non-empty 'reason' — the escape hatch requires \
                         accountability, not silence"
                    .to_string(),
            },
            "advance_phase(force: true) requires a non-empty 'reason'",
        ));
    }

    let project_dir = project_dir_from_board(&board);
    let path = phase_state_path(&project_dir);
    let mut state = read_state(&path).await;
    let current = current_phase_of(&state);

    if target == current {
        return Ok(CallToolResult::json(&json!({
            "changed": false,
            "current_phase": current.as_str(),
            "message": "already in this phase"
        })));
    }

    let is_forward = target > current;
    let unmet = if is_forward && !force {
        unmet_criteria_for(target, &board, ctx).await
    } else {
        Vec::new()
    };

    if !unmet.is_empty() {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::PhaseGateBlocked {
                current_phase: current.as_str().to_string(),
                required_phase: target.as_str().to_string(),
                unmet_criteria: unmet.clone(),
            },
            format!(
                "Cannot advance from '{}' to '{}': {}",
                current.as_str(),
                target.as_str(),
                unmet.join("; ")
            ),
        ));
    }

    let now = iso8601_now();
    let obj = state
        .as_object_mut()
        .expect("read_state always returns an object (default or parsed)");

    {
        let history = ensure_array_field(obj, "phase_history");
        if let Some(last) = history.last_mut() {
            if last["exited_at"].is_null() {
                last["exited_at"] = json!(now);
            }
        }
        history
            .push(json!({ "phase": target.as_str(), "entered_at": now, "exited_at": Value::Null }));
    }

    if is_forward {
        let completed = ensure_array_field(obj, "completed_phases");
        let already = completed
            .iter()
            .any(|v| v.as_str() == Some(current.as_str()));
        if !already {
            completed.push(json!(current.as_str()));
        }
    }

    obj.insert("current_phase".to_string(), json!(target.as_str()));

    if force {
        let overrides = ensure_array_field(obj, "overrides");
        overrides.push(json!({
            "phase": target.as_str(),
            "tool": "advance_phase",
            "reason": reason.unwrap_or_default(),
            "timestamp": now
        }));
    }

    write_state(&path, &state).await?;

    Ok(CallToolResult::json(&json!({
        "changed": true,
        "previous_phase": current.as_str(),
        "current_phase": target.as_str(),
        "forced": force,
        "phase_state": state
    })))
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::sync::Arc;
    use tempfile::TempDir;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../konnect-sexp/tests/fixtures/placement/placement_fixture.kicad_pcb"
    );

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig::default(),
            Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn result_json(result: &CallToolResult) -> Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    /// Writes a minimal valid design_intent.json with one functional block,
    /// bypassing the design_intent toolset's own handlers (those are module-
    /// private) — this is the same file `design_intent.rs`'s own
    /// `set_design_intent` would produce, just written directly since only
    /// the placement-phase gate needs to observe it here.
    async fn seed_functional_block(board: &Path) {
        let project_dir = project_dir_from_board(board);
        let path = project_dir.join(".konnect").join("design_intent.json");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let doc = json!({
            "version": 1,
            "functional_blocks": { "power": { "label": "Power", "components": ["U1"] } },
            "interfaces": {},
            "net_priorities": {},
            "decisions": []
        });
        tokio::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap())
            .await
            .unwrap();
    }

    /// Copies the placement fixture into `tmp`, either as-is (passes
    /// score_placement at 70, verdict "pass" — see placement.rs's own
    /// `kicad_fixture_passes_at_70_with_only_the_decoupling_deduction`) or
    /// with the same C1/C2-overlap string surgery placement.rs's
    /// `overlapping_courtyards_are_a_hard_fail_naming_the_pair` test uses, to
    /// produce a hard_fail verdict.
    fn board_with_verdict(tmp: &TempDir, hard_fail: bool) -> PathBuf {
        let fixture = std::fs::read_to_string(FIXTURE).unwrap();
        let content = if hard_fail {
            assert_eq!(fixture.matches("(at 30 15 90)").count(), 1);
            fixture.replace("(at 30 15 90)", "(at 20 15 90)")
        } else {
            fixture
        };
        let path = tmp.path().join("board.kicad_pcb");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn get_phase_state_defaults_to_analysis_when_no_file_exists() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = tmp.path().join("board.kicad_pcb");

        let result = handle_get_phase_state(&json!({ "board": board.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["exists"], false);
        assert_eq!(body["phase_state"]["current_phase"], "analysis");
        assert_eq!(body["phase_state"]["completed_phases"], json!([]));
        assert_eq!(body["phase_state"]["overrides"], json!([]));
    }

    #[tokio::test]
    async fn advance_phase_refuses_critical_routing_when_placement_hard_fails() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_with_verdict(&tmp, true);

        let result = handle_advance_phase(
            &json!({ "board": board.to_string_lossy(), "target_phase": "critical_routing" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["error"]["kind"], "phase_gate_blocked");
        assert_eq!(body["error"]["current_phase"], "analysis");
        assert_eq!(body["error"]["required_phase"], "critical_routing");
        let unmet = body["error"]["unmet_criteria"].as_array().unwrap();
        assert!(
            unmet
                .iter()
                .any(|c| c.as_str().unwrap().contains("hard_fail")),
            "{unmet:?}"
        );

        // The refusal must not have moved the phase.
        let state = handle_get_phase_state(&json!({ "board": board.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            result_json(&state)["phase_state"]["current_phase"],
            "analysis"
        );
    }

    #[tokio::test]
    async fn advance_phase_allows_critical_routing_when_placement_passes() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_with_verdict(&tmp, false);

        let result = handle_advance_phase(
            &json!({ "board": board.to_string_lossy(), "target_phase": "critical_routing" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["changed"], true);
        assert_eq!(body["previous_phase"], "analysis");
        assert_eq!(body["current_phase"], "critical_routing");
        assert_eq!(body["forced"], false);

        let history = body["phase_state"]["phase_history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["phase"], "analysis");
        assert!(history[0]["exited_at"].is_string(), "{history:?}");
        assert_eq!(history[1]["phase"], "critical_routing");
        assert!(history[1]["exited_at"].is_null());

        let completed = body["phase_state"]["completed_phases"].as_array().unwrap();
        assert_eq!(completed, &vec![json!("analysis")]);
    }

    #[tokio::test]
    async fn advance_phase_force_with_reason_succeeds_and_is_recorded() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_with_verdict(&tmp, true); // would otherwise hard-fail

        let result = handle_advance_phase(
            &json!({
                "board": board.to_string_lossy(),
                "target_phase": "critical_routing",
                "force": true,
                "reason": "Routing the known-clean net first while the overlap fix is reviewed"
            }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["changed"], true);
        assert_eq!(body["forced"], true);

        let overrides = body["phase_state"]["overrides"].as_array().unwrap();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0]["phase"], "critical_routing");
        assert_eq!(overrides[0]["tool"], "advance_phase");
        assert!(overrides[0]["reason"]
            .as_str()
            .unwrap()
            .contains("known-clean net"));
        assert!(overrides[0]["timestamp"].as_str().unwrap().ends_with('Z'));
    }

    #[tokio::test]
    async fn advance_phase_force_without_reason_is_refused() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_with_verdict(&tmp, true);

        let result = handle_advance_phase(
            &json!({
                "board": board.to_string_lossy(),
                "target_phase": "critical_routing",
                "force": true
            }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["error"]["kind"], "invalid_argument");
        assert_eq!(body["error"]["field"], "reason");

        // Still analysis — the refused override must not have moved anything.
        let state = handle_get_phase_state(&json!({ "board": board.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            result_json(&state)["phase_state"]["current_phase"],
            "analysis"
        );
    }

    #[tokio::test]
    async fn moving_backward_never_gates() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_with_verdict(&tmp, true); // would hard-fail forward

        handle_advance_phase(
            &json!({
                "board": board.to_string_lossy(), "target_phase": "critical_routing",
                "force": true, "reason": "test setup"
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Now step back to 'analysis' with no force — must succeed even
        // though a *forward* move would still hard-fail.
        let result = handle_advance_phase(
            &json!({ "board": board.to_string_lossy(), "target_phase": "analysis" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        let body = result_json(&result);
        assert_eq!(body["current_phase"], "analysis");
        assert_eq!(body["forced"], false);
    }

    #[tokio::test]
    async fn advance_phase_rejects_an_unknown_target_phase() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = tmp.path().join("board.kicad_pcb");

        let result = handle_advance_phase(
            &json!({ "board": board.to_string_lossy(), "target_phase": "routing_but_typo" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(result_json(&result)["error"]["kind"], "invalid_argument");
    }

    #[tokio::test]
    async fn advance_phase_to_placement_requires_a_functional_block() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = tmp.path().join("board.kicad_pcb");

        let refused = handle_advance_phase(
            &json!({ "board": board.to_string_lossy(), "target_phase": "placement" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(refused.is_error, "{refused:?}");
        assert_eq!(result_json(&refused)["error"]["kind"], "phase_gate_blocked");

        seed_functional_block(&board).await;

        let allowed = handle_advance_phase(
            &json!({ "board": board.to_string_lossy(), "target_phase": "placement" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!allowed.is_error, "{allowed:?}");
    }

    // ── require_phase_at_least — the function pcb_routing.rs's mutation
    // tools call directly ──────────────────────────────────────────────────

    #[tokio::test]
    async fn require_phase_at_least_refuses_below_the_minimum() {
        let tmp = TempDir::new().unwrap();
        let board = tmp.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();

        let err = require_phase_at_least(&board, Phase::CriticalRouting)
            .await
            .expect_err("analysis phase must not clear a critical_routing minimum");
        let body = result_json(&err);
        assert_eq!(body["error"]["kind"], "phase_gate_blocked");
        assert_eq!(body["error"]["current_phase"], "analysis");
        assert_eq!(body["error"]["required_phase"], "critical_routing");
    }

    #[tokio::test]
    async fn require_phase_at_least_allows_once_advanced_far_enough() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_ctx();
        let board = board_with_verdict(&tmp, false);

        handle_advance_phase(
            &json!({ "board": board.to_string_lossy(), "target_phase": "critical_routing" }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(require_phase_at_least(&board, Phase::CriticalRouting)
            .await
            .is_ok());
        // 'routing' is later than 'critical_routing', so it must also clear
        // a critical_routing minimum.
        handle_advance_phase(
            &json!({ "board": board.to_string_lossy(), "target_phase": "routing", "force": true, "reason": "test" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(require_phase_at_least(&board, Phase::CriticalRouting)
            .await
            .is_ok());
    }
}
