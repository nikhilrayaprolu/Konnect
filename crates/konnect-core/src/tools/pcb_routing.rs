//! `pcb_routing` toolset — traces, vias, copper pours, nets, netclasses, and diff pairs.
//!
//! Routing operations use the KiCAD IPC API; `add_net`, `create_netclass`, and
//! `add_copper_pour` use S-expression file manipulation.
//!
//! `route_trace`, `route_pad_to_pad`, `add_via`, and `route_differential_pair`
//! (the actual copper-mutating tools) each call
//! `phase_gate::require_phase_at_least(board, Phase::CriticalRouting)` before
//! doing anything else, refusing with a structured `phase_gate_blocked` error
//! if the project's persisted workflow phase (`.konnect/phase_state.json`)
//! hasn't reached `critical_routing` yet. This is a deliberately narrow
//! enforcement surface — see `tools/phase_gate.rs`'s module doc for why only
//! these four tools are gated and not the other 220+ in this server.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, opt_f64, require_f64, require_str, with_board_ipc_classified, ToolContext, ToolDef,
};
use anyhow::Context;
use konnect_sexp::writer::{apply_edits, write_atomic, SexpEdit};
use prost::Message;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use super::cli;
use super::phase_gate::{require_phase_at_least, Phase};

macro_rules! ipc {
    ($ctx:expr, $args:expr, |$c:ident| $body:expr) => {{
        let requested_board = get_path($args, "board")?;
        match with_board_ipc_classified($ctx, &requested_board, move |$c| $body).await? {
            Ok(v) => v,
            Err(konnect_ipc::IpcFailure::Target { error, .. }) => {
                return Ok(crate::tools::ipc_target_error_result(&error))
            }
            Err(error) => {
                return Ok(CallToolResult::error(format!(
                    "KiCAD must be running with the board loaded (IPC error: {})",
                    error.message()
                )))
            }
        }
    }};
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_net",
            "Add a new net entry to the top-level net table of a pre-KiCad-10 board \
             (S-expression insert, no KiCAD IPC required). Fails on a KiCad 10 board, which \
             has no net table — there, name the net on copper (route_trace, add_via, \
             add_copper_pour) instead.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" }
                },
                "required": ["board", "net_name"]
            }),
            |args, ctx| async move { handle_add_net(args, ctx).await }
        ),
        tool!(
            "route_trace",
            "Route a trace segment between two points on a copper layer via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" },
                    "layer":    { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_trace(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "route_pad_to_pad",
            "Route a direct trace between two pads of named components (L-bend routing) via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "net_name":    { "type": "string" },
                    "ref1":        { "type": "string", "description": "First component reference" },
                    "pad1":        { "type": "string", "description": "First pad number" },
                    "ref2":        { "type": "string", "description": "Second component reference" },
                    "pad2":        { "type": "string", "description": "Second pad number" },
                    "layer":       { "type": "string", "default": "F.Cu" },
                    "width":       { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "ref1", "pad1", "ref2", "pad2"]
            }),
            |args, ctx| async move { handle_route_pad_to_pad(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "add_via",
            "Add a through-hole via at a given position and assign it to a net via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" },
                    "drill":     { "type": "number", "description": "Drill diameter in mm", "default": 0.4 },
                    "pad_size":  { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 }
                },
                "required": ["board", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_add_via(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "plan_specctra_ses_import",
            "Validate a Freerouting Specctra SES against its revision-bound Konnect manifest and the exact live KiCad board. Returns every track and via that would be created; never mutates or saves the board.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Open source .kicad_pcb used for the DSN export" },
                    "ses_path": { "type": "string", "description": "Freerouting .ses result" },
                    "manifest_path": { "type": "string", "description": "Konnect reverse manifest written with the DSN" }
                },
                "required": ["board", "ses_path", "manifest_path"]
            }),
            |args, ctx| async move { handle_plan_specctra_ses_import(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "apply_specctra_ses",
            "Apply a fully validated Freerouting SES to the exact live KiCad board as one undo transaction, without saving over the source. Creates a new candidate .kicad_pcb, proves IPC read-back counts, and runs KiCad DRC before committing.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Open source .kicad_pcb used for the DSN export" },
                    "ses_path": { "type": "string", "description": "Freerouting .ses result" },
                    "manifest_path": { "type": "string", "description": "Konnect reverse manifest written with the DSN" },
                    "candidate_output_path": { "type": "string", "description": "New .kicad_pcb path. Existing files are never replaced." }
                },
                "required": ["board", "ses_path", "manifest_path", "candidate_output_path"]
            }),
            |args, ctx| async move { handle_apply_specctra_ses(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "add_copper_pour",
            "Alias of pcb_board's add_zone, kept for compatibility: identical arguments, \
             defaults and behaviour. Adds a copper fill zone polygon on a layer/net, trying \
             KiCAD IPC first and falling back to an S-expression file insert (with a warning) \
             only when no live KiCAD answers.",
            crate::tools::pcb_board::zone_schema(),
            |args, ctx| async move { handle_add_copper_pour(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback),
        tool!(
            "delete_trace",
            "Delete a trace segment identified by its UUID via KiCAD IPC. Refuses UUIDs that are not observed trace segments on the requested board, then verifies the segment is absent before reporting success. Returns the observed preimage and postcondition.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the track segment to delete" }
                },
                "required": ["board", "uuid"]
            }),
            |args, ctx| async move { handle_delete_trace(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "query_traces",
            "List trace segments on the board, optionally filtered by net and/or layer. \
             Each result includes the track's UUID, which delete_trace takes.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Filter by net (optional)" },
                    "layer":    { "type": "string", "description": "Filter by layer (optional)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_query_traces(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "get_nets_list",
            "Return all nets defined on the PCB via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_nets_list(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "modify_trace",
            "Modify a trace segment by deleting and re-adding it with new parameters.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "uuid":      { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width":     { "type": "number", "default": 0.25 }
                },
                "required": ["board", "uuid", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_modify_trace(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "create_netclass",
            "Create or update a netclass in the project's design rules. Writes \
             net_settings in the sibling .kicad_pro (where KiCad keeps netclasses \
             since v7); the board file is never touched. Requires the project file \
             to exist. An update changes only the settings you name. To see what a \
             class holds, call get_netclasses rather than this tool: naming a class \
             that does not exist here creates it with the defaults, so a call meant \
             as a look writes one instead — and its result is nearly \
             indistinguishable from a read of an existing class. The class \
             named 'Default' is special: it is written complete, because KiCad \
             replaces its own default with it and backfills every other class \
             from it. Calling this on an existing, incomplete 'Default' \
             repairs it in place without touching values already set.",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is edited" },
                    "name":         { "type": "string", "description": "Netclass name (e.g. 'Power')" },
                    "clearance":    { "type": "number", "description": "Clearance in mm", "default": 0.2 },
                    "trace_width":  { "type": "number", "description": "Default trace width in mm", "default": 0.25 },
                    "via_drill":    { "type": "number", "description": "Via drill diameter in mm", "default": 0.4 },
                    "via_diameter": { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 }
                },
                "required": ["board", "name"]
            }),
            |args, ctx| async move { handle_create_netclass(args, ctx).await }
        ),
        tool!(
            "get_netclasses",
            "Read every netclass in the project's design rules, with its settings, \
             its netclass_patterns and the board nets those patterns match. Reads \
             the sibling .kicad_pro and the board file; KiCad need not be running. \
             Call before create_netclass to see what a class holds — an update \
             changes only the values you name, and this is the only way to see the \
             rest. A net can match several classes: KiCad takes each property from \
             the highest-priority class that sets it (lower number = higher \
             priority) and falls back to Default. Settings are reported \
             resolved, with 'inherits' naming the ones a class takes from the \
             Default rather than setting itself; 'missing_fields' on the \
             Default names settings nothing can resolve.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is read" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_netclasses(args, ctx).await }
        ),
        tool!(
            "assign_net_to_class",
            "Assign a net to an existing netclass, as a netclass_patterns entry in \
             the sibling .kicad_pro. The class must already exist (create_netclass). \
             Reassigning moves the net's entry to the new class.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is edited" },
                    "net_name":  { "type": "string", "description": "Net name to assign" },
                    "netclass":  { "type": "string", "description": "Netclass name to assign the net to" }
                },
                "required": ["board", "net_name", "netclass"]
            }),
            |args, ctx| async move { handle_assign_net_to_class(args, ctx).await }
        ),
        tool!(
            "route_differential_pair",
            "Route a differential pair (two parallel traces with a specified gap).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_pos":  { "type": "string", "description": "Positive net name" },
                    "net_neg":  { "type": "string", "description": "Negative net name" },
                    "layer":    { "type": "string", "default": "F.Cu" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.1 },
                    "gap":   { "type": "number", "description": "Gap between pair traces in mm", "default": 0.1 }
                },
                "required": ["board", "net_pos", "net_neg", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_diff_pair(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "delete_via",
            "Delete a via identified by its UUID via KiCAD IPC. Refuses UUIDs that are not \
             observed vias on the requested board (including a trace segment's UUID — vias and \
             traces share KiCad's KOT_PCB_TRACE family selector, so the type is checked before \
             the delete), then verifies the via is absent before reporting success. Returns the \
             observed preimage and postcondition.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the via to delete" }
                },
                "required": ["board", "uuid"]
            }),
            |args, ctx| async move { handle_delete_via(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "move_via",
            "Move a via identified by its UUID to a new position via KiCAD IPC. Refuses UUIDs \
             that are not observed vias on the requested board, then verifies the new position \
             is reported by a fresh read-back before reporting success. Returns the observed \
             preimage and postimage.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the via to move" },
                    "x": { "type": "number", "description": "New X position in mm" },
                    "y": { "type": "number", "description": "New Y position in mm" }
                },
                "required": ["board", "uuid", "x", "y"]
            }),
            |args, ctx| async move { handle_move_via(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "query_vias",
            "List vias on the board, optionally filtered by net. Each result includes the \
             via's UUID, which delete_via and move_via take.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Filter by net (optional)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_query_vias(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "check_route_collision",
            "Pre-flight collision check for a proposed straight trace segment, run before \
             route_trace / route_pad_to_pad actually create copper. route_trace does no \
             collision or clearance checking of its own, so a straight point-to-point trace \
             over an existing board is not proven clear of other copper. This queries the \
             requested board's existing tracks (same layer), vias (KiCad's KOT_PCB_TRACE \
             family selector does not separate layers for vias, so every via is checked \
             regardless of layer — a safe over-approximation, since Konnect's own add_via only \
             creates through vias spanning every layer), and every placed footprint's pads on \
             the requested layer via KiCAD IPC, runs 2D segment-to-segment and point-to-segment \
             distance checks against each different-net item (a pad is approximated as a circle \
             of radius max(size_x, size_y)/2 — conservative, not exact, since this is a \
             refuse-if-unsure check), and resolves the required clearance from the board's \
             effective netclass rules (falling back to KiCad's stock 0.2 mm Default when neither \
             net's clearance is resolvable). Returns 'clear': true or a structured list of \
             conflicts — the offending item (track/via/pad, with the pad's owning reference \
             designator and number), its net, the clearance the design rules require, and the \
             clearance the geometry actually leaves — so the caller can choose a different path \
             instead of guessing why a route was refused. Same-net items and unconnected \
             (netless) pads are never conflicts. This is not a full DRC pass: zones, rotated \
             non-circular pad shapes, and non-straight geometry are still out of scope — pad \
             checking is a circular approximation, not exact rectangle/rounded-rect geometry.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Net the proposed segment belongs to; same-net items are not conflicts" },
                    "layer":    { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_check_route_collision(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
        tool!(
            "classify_nets",
            "Heuristically classify every net on the board as power, ground, differential_pair, \
             or signal. Read-only and derived — not a stored data model. 'power' matches the \
             configured net_prefix_power naming convention (from get_effective_config, or the \
             'net_prefix_power' argument to override it for this call); 'ground' matches \
             net_prefix_ground plus common ground-net spellings (GND, GROUND, AGND, DGND, PGND, \
             EARTH, 0V); 'differential_pair' detects nets paired by a recognized complementary \
             suffix (_P/_N, _DP/_DM, _DP/_DN, _PLUS/_MINUS, or trailing +/-) where both halves \
             exist on the board, e.g. USB_DP/USB_DM or CLK_P/CLK_N; everything else is 'signal'. \
             Returns each net's class and, for detected pairs, the positive/negative net names.",
            json!({
                "type": "object",
                "properties": {
                    "board":             { "type": "string" },
                    "net_prefix_power":  { "type": "string", "description": "Override the power-net prefix; defaults to the effective config's naming_conventions.net_prefix_power" },
                    "net_prefix_ground": { "type": "string", "description": "Override the ground-net prefix; defaults to the effective config's naming_conventions.net_prefix_ground" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_classify_nets(args, ctx).await }
        )
        .with_board_access(crate::tools::BoardAccess::LiveOnly),
    ]
}

// ─── Geometry (check_route_collision) ──────────────────────────────────────────

/// Distance in mm from point `p` to the segment `a`-`b`.
fn point_segment_distance_mm(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len_sq = dx * dx + dy * dy;
    if len_sq < 1e-12 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len_sq).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// Signed area of the triangle a-b-c, used to test which side of line a-b
/// point c falls on. Zero means collinear.
fn orientation((ax, ay): (f64, f64), (bx, by): (f64, f64), (cx, cy): (f64, f64)) -> f64 {
    (bx - ax) * (cy - ay) - (by - ay) * (cx - ax)
}

/// Whether `p`, known collinear with `a`-`b`, lies within its bounding box.
fn on_segment(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> bool {
    const EPS: f64 = 1e-9;
    p.0 >= a.0.min(b.0) - EPS
        && p.0 <= a.0.max(b.0) + EPS
        && p.1 >= a.1.min(b.1) - EPS
        && p.1 <= a.1.max(b.1) + EPS
}

/// Whether segments `a1`-`a2` and `b1`-`b2` intersect or touch, including the
/// collinear-overlap case.
fn segments_intersect(a1: (f64, f64), a2: (f64, f64), b1: (f64, f64), b2: (f64, f64)) -> bool {
    const EPS: f64 = 1e-9;
    let d1 = orientation(b1, b2, a1);
    let d2 = orientation(b1, b2, a2);
    let d3 = orientation(a1, a2, b1);
    let d4 = orientation(a1, a2, b2);
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        return true;
    }
    (d1.abs() < EPS && on_segment(b1, b2, a1))
        || (d2.abs() < EPS && on_segment(b1, b2, a2))
        || (d3.abs() < EPS && on_segment(a1, a2, b1))
        || (d4.abs() < EPS && on_segment(a1, a2, b2))
}

/// Minimum distance in mm between segments `a1`-`a2` and `b1`-`b2`; `0.0` when
/// they intersect or touch.
fn segment_segment_distance_mm(
    a1: (f64, f64),
    a2: (f64, f64),
    b1: (f64, f64),
    b2: (f64, f64),
) -> f64 {
    if segments_intersect(a1, a2, b1, b2) {
        return 0.0;
    }
    point_segment_distance_mm(a1, b1, b2)
        .min(point_segment_distance_mm(a2, b1, b2))
        .min(point_segment_distance_mm(b1, a1, a2))
        .min(point_segment_distance_mm(b2, a1, a2))
}

/// KiCad's stock Default netclass clearance (mm), used only when neither side
/// of a checked pair resolves a clearance from `get_effective_routing_rules_in`
/// — matching `kicad_default_class`'s `"clearance": 0.2` elsewhere in this file.
const FALLBACK_CLEARANCE_MM: f64 = 0.2;

/// The clearance required between two nets: the larger of whichever effective
/// clearances are resolvable, or the KiCad stock default when neither is.
fn required_clearance_mm(
    rules: &konnect_ipc::IpcEffectiveRoutingRules,
    net_a: &str,
    net_b: &str,
) -> f64 {
    let a = rules.get(net_a).and_then(|r| r.clearance_mm);
    let b = rules.get(net_b).and_then(|r| r.clearance_mm);
    match (a, b) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => FALLBACK_CLEARANCE_MM,
    }
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_net(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parse_sexp(&content)?;

    if board_is_kicad_10(&tree) {
        return Ok(CallToolResult::error(format!(
            "Cannot add net '{net_name}' to this board: it is in the KiCad 10 format, which has \
             no top-level net table. A net exists only by being named on an item — \
             (net \"{net_name}\") on a pad, segment, via or zone — so there is nothing for a \
             file-level insert to add, and appending a net node would report success while \
             KiCad discarded it on load. Create the net by naming it on copper instead: \
             route_trace / add_via / add_copper_pour take a net_name, as does assigning a pad \
             in KiCad. get_nets_list reads the live net list over IPC."
        )));
    }

    // Pre-KiCad-10: the top-level table is real, so an insert is meaningful.
    // The next id is one past the highest in use — not the number of "(net "
    // occurrences in the file, which counted every reference on every pad,
    // segment and zone and so collided with existing ids almost immediately.
    let net_id = tree
        .find_all("net")
        .iter()
        .filter_map(|n| konnect_sexp::net::net_id(n))
        .filter_map(|id| id.parse::<i32>().ok())
        .max()
        .map(|max| max + 1)
        .unwrap_or(1);
    let net_sexp = format!("\n  (net {net_id} \"{net_name}\")");
    // Insert before the last closing paren
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, net_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net_id": net_id, "net_name": net_name }),
    ))
}

/// Whether a board is in the KiCad 10 format, where nets are implicit.
///
/// The detection (shape first, version fallback) moved to
/// [`konnect_sexp::net::names_nets_in_place`] so the write side (#192) shares
/// it; this wrapper keeps the call sites readable.
fn board_is_kicad_10(tree: &konnect_sexp::SexpNode) -> bool {
    konnect_sexp::net::names_nets_in_place(tree)
}

async fn handle_route_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_for_gate = get_path(args, "board")?;
    if let Err(error) = require_phase_at_least(&board_for_gate, Phase::CriticalRouting).await {
        return Ok(error);
    }
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.25);

    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, args, |c| c
        .add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2));
    Ok(CallToolResult::json(&json!({
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 }
    })))
}

async fn handle_route_pad_to_pad(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    if let Err(error) = require_phase_at_least(&board_path, Phase::CriticalRouting).await {
        return Ok(error);
    }
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad1 = match require_str(args, "pad1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad2 = match require_str(args, "pad2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let width = args["width"].as_f64().unwrap_or(0.25);

    // Look up pad positions from the PCB S-expression file
    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    let pos1 = find_pad_board_position(&tree, &ref1, &pad1)?;
    let pos2 = find_pad_board_position(&tree, &ref2, &pad2)?;

    // Route an L-bend: horizontal first, then vertical
    let (x1, y1) = pos1;
    let (x2, y2) = pos2;
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();

    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        // Already axis-aligned: single segment
        ipc!(ctx, args, |c| c
            .add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2));
    } else {
        // L-bend: horizontal then vertical
        let mid_x = x2;
        let mid_y = y1;
        let net_a = net_name.clone();
        let net_b = net_name.clone();
        let layer_a = layer.clone();
        let layer_b = layer.clone();
        ipc!(ctx, args, |c| {
            c.add_track(&net_a, &layer_a, width, x1, y1, mid_x, mid_y)?;
            c.add_track(&net_b, &layer_b, width, mid_x, mid_y, x2, y2)?;
            Ok(())
        });
    }

    Ok(CallToolResult::json(&json!({
        "routed": true,
        "net": net_name, "layer": layer, "width": width,
        "from": { "ref": ref1, "pad": pad1, "x": x1, "y": y1 },
        "to":   { "ref": ref2, "pad": pad2, "x": x2, "y": y2 }
    })))
}

/// Look up a pad's board-space (x, y) position from the parsed PCB S-expression tree.
fn find_pad_board_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
    pad_number: &str,
) -> anyhow::Result<(f64, f64)> {
    let fp_node = tree
        .find_all("footprint")
        .into_iter()
        .find(|fp| {
            fp.find_all("property").iter().any(|p| {
                p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                    && p.get(2).and_then(|n| n.as_str()) == Some(reference)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found on board", reference))?;

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pad = fp_node
        .find_all("pad")
        .into_iter()
        .find(|p| p.get(1).and_then(|n| n.as_str()) == Some(pad_number))
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found on '{}'", pad_number, reference))?;

    let pad_at = pad
        .find("at")
        .ok_or_else(|| anyhow::anyhow!("Pad has no (at) node"))?;
    let local_x = pad_at.get_f64(1).unwrap_or(0.0);
    let local_y = pad_at.get_f64(2).unwrap_or(0.0);

    // Transform local pad coords to board space (rotation only).
    // Uses the canonical KiCAD transform — see konnect_sexp::geometry.
    Ok(konnect_sexp::geometry::transform_pad(
        local_x, local_y, fp_x, fp_y, fp_rot,
    ))
}

async fn handle_add_via(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_for_gate = get_path(args, "board")?;
    if let Err(error) = require_phase_at_least(&board_for_gate, Phase::CriticalRouting).await {
        return Ok(error);
    }
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let drill = args["drill"].as_f64().unwrap_or(0.4);
    let pad_size = args["pad_size"].as_f64().unwrap_or(0.8);

    let net_ipc = net_name.clone();
    ipc!(ctx, args, |c| c.add_via(&net_ipc, x, y, drill, pad_size));
    Ok(CallToolResult::json(
        &json!({ "net": net_name, "x": x, "y": y, "drill": drill, "pad_size": pad_size }),
    ))
}

fn extension_is(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn invalid_specctra_argument(name: &str, reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::InvalidArgument {
            field: name.to_string(),
            reason: reason.to_string(),
        },
        format!("Invalid '{name}': {reason}"),
    )
}

async fn read_specctra_inputs(
    args: &serde_json::Value,
) -> anyhow::Result<Result<(std::path::PathBuf, String, String), CallToolResult>> {
    let ses_path = get_path(args, "ses_path")?;
    let manifest_path = get_path(args, "manifest_path")?;
    if !extension_is(&ses_path, "ses") {
        return Ok(Err(invalid_specctra_argument(
            "ses_path",
            "must have the .ses extension",
        )));
    }
    if !manifest_path.is_file() {
        return Ok(Err(invalid_specctra_argument(
            "manifest_path",
            "must name an existing reverse-manifest JSON file",
        )));
    }
    let ses_source = tokio::fs::read_to_string(&ses_path).await?;
    let manifest_source = tokio::fs::read_to_string(&manifest_path).await?;
    Ok(Ok((ses_path, ses_source, manifest_source)))
}

async fn handle_plan_specctra_ses_import(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    if !extension_is(&board, "kicad_pcb") {
        return Ok(invalid_specctra_argument(
            "board",
            "must have the .kicad_pcb extension",
        ));
    }
    let (_ses_path, ses_source, manifest_source) = match read_specctra_inputs(args).await? {
        Ok(inputs) => inputs,
        Err(error) => return Ok(error),
    };
    let board = board
        .canonicalize()
        .with_context(|| format!("resolve board {}", board.display()))?;
    let board_for_ipc = board.clone();
    let result = with_board_ipc_classified(ctx, &board, move |client| {
        let document = client.find_open_board(&board_for_ipc)?;
        let before = client.save_document_to_string_in(document.clone())?;
        let plan = crate::specctra_ses::parse_import_plan(
            &board_for_ipc,
            &before,
            &manifest_source,
            &ses_source,
        )?;
        let after = client.save_document_to_string_in(document)?;
        if before != after {
            anyhow::bail!("KiCad board changed while the SES import was planned; retry from a stable editor revision");
        }
        Ok(plan)
    })
    .await?;
    match result {
        Ok(plan) => Ok(CallToolResult::json(&json!({
            "success": true,
            "method": "strict_dry_run",
            "board": board,
            "source_sha256": plan.source_sha256,
            "session_id": plan.session_id,
            "track_count": plan.tracks.len(),
            "arc_count": plan.arcs.len(),
            "via_count": plan.vias.len(),
            "preserved_locked_routing": {
                "tracks": plan.locked_track_count,
                "vias": plan.locked_via_count
            },
            "tracks": plan.tracks,
            "arcs": plan.arcs,
            "vias": plan.vias,
            "mutated": false
        }))),
        Err(failure) => {
            let reason = failure.message().to_string();
            Ok(CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: reason.clone(),
                },
                format!("Specctra SES import refused: {reason}"),
            ))
        }
    }
}

#[derive(Debug)]
struct ApplyEvidence {
    source_sha256: String,
    session_id: String,
    track_count: usize,
    arc_count: usize,
    via_count: usize,
    created_count: usize,
    preserved_locked_track_count: usize,
    preserved_locked_via_count: usize,
    drc_violations: usize,
    unconnected_items: usize,
    schematic_parity_violations: usize,
}

fn created_route_item_ids(items: &[prost_types::Any]) -> anyhow::Result<Vec<String>> {
    use konnect_ipc::gen::kiapi::board::types::{Arc, Track, Via};

    let mut ids = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let id = if item.type_url.ends_with("kiapi.board.types.Track") {
            Track::decode(item.value.as_slice())?.id
        } else if item.type_url.ends_with("kiapi.board.types.Arc") {
            Arc::decode(item.value.as_slice())?.id
        } else if item.type_url.ends_with("kiapi.board.types.Via") {
            Via::decode(item.value.as_slice())?.id
        } else {
            anyhow::bail!(
                "KiCad returned unexpected created item type '{}' at index {index}",
                item.type_url
            );
        }
        .with_context(|| format!("KiCad returned created route item {index} without a KIID"))?
        .value;
        if id.is_empty() {
            anyhow::bail!("KiCad returned created route item {index} with an empty KIID");
        }
        ids.push(id);
    }
    if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        anyhow::bail!("KiCad returned duplicate KIIDs for created route items");
    }
    Ok(ids)
}

async fn handle_apply_specctra_ses(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let candidate = get_path(args, "candidate_output_path")?;
    if !extension_is(&board, "kicad_pcb") {
        return Ok(invalid_specctra_argument(
            "board",
            "must have the .kicad_pcb extension",
        ));
    }
    if !extension_is(&candidate, "kicad_pcb") {
        return Ok(invalid_specctra_argument(
            "candidate_output_path",
            "must have the .kicad_pcb extension",
        ));
    }
    let drc_output = candidate.with_extension("drc.json");
    let conflicts = [&candidate, &drc_output]
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if !conflicts.is_empty() {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::Conflict {
                paths: conflicts.clone(),
            },
            format!(
                "Specctra import is non-destructive; candidate or DRC output already exists: {}",
                conflicts.join(", ")
            ),
        ));
    }
    if let Some(parent) = candidate
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let (_ses_path, ses_source, manifest_source) = match read_specctra_inputs(args).await? {
        Ok(inputs) => inputs,
        Err(error) => return Ok(error),
    };
    let board = board
        .canonicalize()
        .with_context(|| format!("resolve board {}", board.display()))?;
    let board_for_ipc = board.clone();
    let candidate_for_ipc = candidate.clone();
    let drc_output_for_ipc = drc_output.clone();
    let cli_path = ctx.config.kicad_cli.clone();
    let runtime = tokio::runtime::Handle::current();

    let result = with_board_ipc_classified(ctx, &board, move |client| {
        let open_boards = client.get_open_board_paths()?;
        if open_boards.len() != 1 {
            anyhow::bail!(
                "atomic SES import requires exactly one PCB open in KiCad, got {} ({})",
                open_boards.len(),
                open_boards
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let document = client.find_open_board(&board_for_ipc)?;
        let before = client.save_document_to_string_in(document.clone())?;
        let plan = crate::specctra_ses::parse_import_plan(
            &board_for_ipc,
            &before,
            &manifest_source,
            &ses_source,
        )?;

        use konnect_ipc::gen::kiapi::common::types::KiCadObjectType as ObjectType;
        let existing_tracks = client.get_items_in(document.clone(), ObjectType::KotPcbTrace)?;
        let existing_arcs = client.get_items_in(document.clone(), ObjectType::KotPcbArc)?;
        let existing_vias = client.get_items_in(document.clone(), ObjectType::KotPcbVia)?;
        if existing_tracks.len() != plan.locked_track_count
            || !existing_arcs.is_empty()
            || existing_vias.len() != plan.locked_via_count
        {
            anyhow::bail!(
                "live locked-routing inventory changed: manifest has {} track(s)/{} via(s), IPC read {} track(s)/{} arc(s)/{} via(s)",
                plan.locked_track_count,
                plan.locked_via_count,
                existing_tracks.len(),
                existing_arcs.len(),
                existing_vias.len()
            );
        }
        let net_codes = client
            .get_nets_in(document.clone())?
            .into_iter()
            .map(|net| (net.name, net.netcode))
            .collect::<BTreeMap<_, _>>();
        let mut items = Vec::with_capacity(plan.tracks.len() + plan.arcs.len() + plan.vias.len());
        for track in &plan.tracks {
            konnect_ipc::builders::try_layer_from_name(&track.layer)?;
            let net_code = *net_codes
                .get(&track.net_name)
                .with_context(|| format!("live board has no net '{}'", track.net_name))?;
            let item = konnect_ipc::builders::build_track(
                &track.net_name,
                net_code,
                &track.layer,
                track.width_mm,
                track.x1_mm,
                track.y1_mm,
                track.x2_mm,
                track.y2_mm,
            );
            items.push(konnect_ipc::builders::pack_any(
                &item,
                "kiapi.board.types.Track",
            ));
        }
        for arc in &plan.arcs {
            konnect_ipc::builders::try_layer_from_name(&arc.layer)?;
            let net_code = *net_codes
                .get(&arc.net_name)
                .with_context(|| format!("live board has no net '{}'", arc.net_name))?;
            let item = konnect_ipc::builders::build_track_arc(
                &arc.net_name, net_code, &arc.layer, arc.width_mm,
                arc.start_x_mm, arc.start_y_mm, arc.mid_x_mm, arc.mid_y_mm,
                arc.end_x_mm, arc.end_y_mm,
            );
            items.push(konnect_ipc::builders::pack_any(&item, "kiapi.board.types.Arc"));
        }
        for via in &plan.vias {
            let net_code = *net_codes
                .get(&via.net_name)
                .with_context(|| format!("live board has no net '{}'", via.net_name))?;
            let item = konnect_ipc::builders::build_via(
                &via.net_name,
                net_code,
                via.x_mm,
                via.y_mm,
                via.drill_mm,
                via.size_mm,
            );
            items.push(konnect_ipc::builders::pack_any(
                &item,
                "kiapi.board.types.Via",
            ));
        }
        let stable = client.save_document_to_string_in(document.clone())?;
        if stable != before {
            anyhow::bail!("KiCad board changed while route items were prepared; retry from a stable editor revision");
        }
        let expected_count = items.len();
        let created_ids = client.run_commit("Import Freerouting SES", |client| {
            let created = client.create_items_in_returning(document.clone(), items)?;
            if created.len() != expected_count {
                anyhow::bail!(
                    "KiCad returned {} created items for {} planned route primitives",
                    created.len(),
                    expected_count
                );
            }
            created_route_item_ids(&created)
        })?;

        // KiCad 10 publishes neither GetItems nor SaveDocumentToString changes
        // while a commit is open. End the single user-visible undo transaction,
        // then validate the exact live inventory and serialized candidate. If
        // any post-commit gate fails, delete only the KIIDs returned by
        // CreateItems in a compensating transaction and prove the original
        // serialized board was restored.
        let operation = (|| {
            let read_tracks = client.get_items_in(document.clone(), ObjectType::KotPcbTrace)?;
            let read_arcs = client.get_items_in(document.clone(), ObjectType::KotPcbArc)?;
            let read_vias = client.get_items_in(document.clone(), ObjectType::KotPcbVia)?;
            if read_tracks.len() != plan.locked_track_count + plan.tracks.len()
                || read_arcs.len() != plan.arcs.len()
                || read_vias.len() != plan.locked_via_count + plan.vias.len()
            {
                anyhow::bail!(
                    "post-commit IPC read-back mismatch: expected {} tracks/{} arcs/{} vias, read {} tracks/{} arcs/{} vias",
                    plan.locked_track_count + plan.tracks.len(),
                    plan.arcs.len(),
                    plan.locked_via_count + plan.vias.len(),
                    read_tracks.len(), read_arcs.len(), read_vias.len()
                );
            }
            let candidate_source = client.save_document_to_string_in(document.clone())?;
            konnect_sexp::write_new_atomic(&candidate_for_ipc, &candidate_source)
                .with_context(|| format!("create candidate {}", candidate_for_ipc.display()))?;
            let drc = runtime.block_on(cli::run_drc(&cli_path, &candidate_for_ipc, false))?;
            let parity_count = drc.schematic_parity.as_ref().map_or(0, Vec::len);
            Ok(ApplyEvidence {
                source_sha256: plan.source_sha256.clone(),
                session_id: plan.session_id.clone(),
                track_count: plan.tracks.len(),
                arc_count: plan.arcs.len(),
                via_count: plan.vias.len(),
                created_count: created_ids.len(),
                preserved_locked_track_count: plan.locked_track_count,
                preserved_locked_via_count: plan.locked_via_count,
                drc_violations: drc.violations.len(),
                unconnected_items: drc.unconnected_items.as_ref().map_or(0, Vec::len),
                schematic_parity_violations: parity_count,
            })
        })();
        match operation {
            Ok(evidence) => Ok(evidence),
            Err(error) => {
                let rollback = client.run_commit("Rollback failed Freerouting SES import", |client| {
                    client.delete_items_in(document.clone(), created_ids.clone())
                });
                if let Err(rollback_error) = rollback {
                    anyhow::bail!(
                        "SES import failed ({error}); compensating deletion also failed ({rollback_error})"
                    );
                }
                let restored = client.save_document_to_string_in(document.clone())?;
                if restored != before {
                    anyhow::bail!(
                        "SES import failed ({error}); compensating deletion completed but the live board did not return to its exact pre-import serialization"
                    );
                }
                if candidate_for_ipc.exists() {
                    std::fs::remove_file(&candidate_for_ipc).with_context(|| {
                        format!(
                            "SES import failed ({error}); also failed to remove candidate {}",
                            candidate_for_ipc.display()
                        )
                    })?;
                }
                if drc_output_for_ipc.exists() {
                    std::fs::remove_file(&drc_output_for_ipc).with_context(|| {
                        format!(
                            "SES import failed ({error}); also failed to remove DRC output {}",
                            drc_output_for_ipc.display()
                        )
                    })?;
                }
                Err(error)
            }
        }
    })
    .await?;

    match result {
        Ok(evidence) => Ok(CallToolResult::json(&json!({
            "success": true,
            "method": "strict_atomic_kicad_ipc_import",
            "board": board,
            "candidate_output_path": candidate,
            "source_overwritten": false,
            "undo_description": "Import Freerouting SES",
            "source_sha256": evidence.source_sha256,
            "session_id": evidence.session_id,
            "track_count": evidence.track_count,
            "arc_count": evidence.arc_count,
            "via_count": evidence.via_count,
            "created_count": evidence.created_count,
            "preserved_locked_routing": {
                "tracks": evidence.preserved_locked_track_count,
                "vias": evidence.preserved_locked_via_count
            },
            "ipc_readback": "exact_count_match",
            "drc": {
                "clean": evidence.drc_violations == 0
                    && evidence.unconnected_items == 0
                    && evidence.schematic_parity_violations == 0,
                "violations": evidence.drc_violations,
                "unconnected_items": evidence.unconnected_items,
                "schematic_parity_violations": evidence.schematic_parity_violations
            }
        }))),
        Err(failure) => {
            let reason = failure.message().to_string();
            Ok(CallToolResult::error_kind(
                ToolErrorKind::HandlerError {
                    reason: reason.clone(),
                },
                format!("Specctra SES import refused or rolled back: {reason}"),
            ))
        }
    }
}

/// `add_copper_pour` is an alias of `add_zone`; both build the same zone
/// through [`crate::tools::pcb_board::add_zone_impl`]. They were two
/// near-identical copies that had already drifted (different `min_width`
/// defaults, and the #192 net-lookup bug had to be fixed twice).
async fn handle_add_copper_pour(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    crate::tools::pcb_board::add_zone_impl(args, ctx).await
}

async fn handle_delete_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let board_ipc = board.clone();
    let uuid_ipc = uuid.clone();
    let deleted = match with_board_ipc_classified(ctx, &board, move |client| {
        client.delete_trace_segment_verified(&board_ipc, &uuid_ipc)
    })
    .await?
    {
        Ok(deleted) => deleted,
        Err(error) => {
            return Ok(CallToolResult::error(format!(
                "KiCAD must be running with the board loaded (IPC error: {})",
                error.message()
            )))
        }
    };

    let Some(trace) = deleted else {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: uuid,
                reason: "the UUID is not an observed trace segment on the requested board"
                    .to_string(),
            },
            "The requested UUID is not a trace segment on the requested board. No board item was deleted.",
        ));
    };

    Ok(CallToolResult::json(&json!({
        "deleted_uuid": trace.uuid,
        "deleted_type": "trace_segment",
        "preimage": {
            "uuid": trace.uuid,
            "net": trace.net_name,
            "layer": trace.layer,
            "width": trace.width,
            "from": { "x": trace.start.x, "y": trace.start.y },
            "to": { "x": trace.end.x, "y": trace.end.y }
        },
        "postcondition": "absent_from_trace_readback"
    })))
}

#[cfg(test)]
mod delete_trace_tests {
    use super::*;
    use crate::tools::pcb_board::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use konnect_ipc::gen::kiapi;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn non_trace_and_missing_uuids_refuse_before_delete_items() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        let original = b"(kicad_pcb (version 20260206))";
        std::fs::write(&board, original).unwrap();
        let delete_count = Arc::new(Mutex::new(0usize));
        let delete_count_in_mock = delete_count.clone();
        let address = spawn_kicad_holding_board(&board, move |command| {
            if command.type_url.ends_with("GetItems") {
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items: vec![],
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ));
            }
            if command.type_url.ends_with("DeleteItems") {
                *delete_count_in_mock.lock().unwrap() += 1;
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::DeleteItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        deleted_items: vec![],
                    },
                    "kiapi.common.commands.DeleteItemsResponse",
                ));
            }
            None
        });

        let ctx = ctx_talking_to(address);
        for uuid in ["via-1", "zone-1", "graphic-1", "footprint-1", "missing-1"] {
            let result = handle_delete_trace(
                &json!({ "board": board.to_string_lossy(), "uuid": uuid }),
                &ctx,
            )
            .await
            .unwrap();

            assert!(result.is_error, "{uuid} unexpectedly succeeded");
            assert_eq!(
                crate::mcp::error::extract_error_kind(&result).as_deref(),
                Some("stale_target"),
                "wrong error for {uuid}"
            );
        }
        assert_eq!(*delete_count.lock().unwrap(), 0);
        assert_eq!(std::fs::read(&board).unwrap(), original);
    }

    #[tokio::test]
    async fn success_reports_the_observed_trace_and_verified_postcondition() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
        let deleted = Arc::new(Mutex::new(false));
        let deleted_in_mock = deleted.clone();
        let mut track =
            konnect_ipc::builders::build_track("GND", 7, "F.Cu", 0.4, 1.0, 2.0, 3.0, 4.0);
        track.id = Some(kiapi::common::types::Kiid {
            value: "segment-1".to_string(),
        });
        let packed_track = konnect_ipc::builders::pack_any(&track, "kiapi.board.types.Track");
        let address = spawn_kicad_holding_board(&board, move |command| {
            if command.type_url.ends_with("GetItems") {
                let items = if *deleted_in_mock.lock().unwrap() {
                    vec![]
                } else {
                    vec![packed_track.clone()]
                };
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items,
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ));
            }
            if command.type_url.ends_with("DeleteItems") {
                *deleted_in_mock.lock().unwrap() = true;
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::DeleteItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        deleted_items: vec![],
                    },
                    "kiapi.common.commands.DeleteItemsResponse",
                ));
            }
            None
        });

        let result = handle_delete_trace(
            &json!({ "board": board.to_string_lossy(), "uuid": "segment-1" }),
            &ctx_talking_to(address),
        )
        .await
        .unwrap();

        assert!(!result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["deleted_uuid"], json!("segment-1"));
        assert_eq!(body["deleted_type"], json!("trace_segment"));
        assert_eq!(body["preimage"]["net"], json!("GND"));
        assert_eq!(body["preimage"]["layer"], json!("F.Cu"));
        assert_eq!(body["preimage"]["width"], json!(0.4));
        assert_eq!(body["preimage"]["from"], json!({ "x": 1.0, "y": 2.0 }));
        assert_eq!(body["preimage"]["to"], json!({ "x": 3.0, "y": 4.0 }));
        assert_eq!(body["postcondition"], json!("absent_from_trace_readback"));
    }
}

async fn handle_query_traces(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net = args["net_name"].as_str().map(String::from);
    let layer = args["layer"].as_str().map(String::from);

    let tracks = ipc!(ctx, args, |c| {
        c.get_tracks(net.as_deref(), layer.as_deref())
    });

    let items: Vec<serde_json::Value> = tracks
        .iter()
        .map(|t| {
            json!({
                "uuid": t.uuid,
                "net": t.net_name, "layer": t.layer, "width": t.width,
                "x1": t.start.x, "y1": t.start.y,
                "x2": t.end.x,   "y2": t.end.y
            })
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "traces": items }),
    ))
}

async fn handle_delete_via(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let board_ipc = board.clone();
    let uuid_ipc = uuid.clone();
    let deleted = match with_board_ipc_classified(ctx, &board, move |client| {
        client.delete_via_verified(&board_ipc, &uuid_ipc)
    })
    .await?
    {
        Ok(deleted) => deleted,
        Err(error) => {
            return Ok(CallToolResult::error(format!(
                "KiCAD must be running with the board loaded (IPC error: {})",
                error.message()
            )))
        }
    };

    let Some(via) = deleted else {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: uuid,
                reason: "the UUID is not an observed via on the requested board".to_string(),
            },
            "The requested UUID is not a via on the requested board. No board item was deleted.",
        ));
    };

    Ok(CallToolResult::json(&json!({
        "deleted_uuid": via.uuid,
        "deleted_type": "via",
        "preimage": {
            "uuid": via.uuid,
            "net": via.net_name,
            "position": { "x": via.position.x, "y": via.position.y },
            "drill": via.drill,
            "pad_diameter": via.pad_diameter,
            "via_type": via.via_type
        },
        "postcondition": "absent_from_via_readback"
    })))
}

#[cfg(test)]
mod delete_via_tests {
    use super::*;
    use crate::tools::pcb_board::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use konnect_ipc::gen::kiapi;
    use std::sync::{Arc, Mutex};

    /// A packed `kiapi.board.types.Via` Any, ready to hand back from a mock
    /// `GetItems` response.
    fn packed_via(uuid: &str, net_name: &str, net_code: i32, x: f64, y: f64) -> prost_types::Any {
        let mut via = konnect_ipc::builders::build_via(net_name, net_code, x, y, 0.4, 0.8);
        via.id = Some(kiapi::common::types::Kiid {
            value: uuid.to_string(),
        });
        konnect_ipc::builders::pack_any(&via, "kiapi.board.types.Via")
    }

    /// A packed `kiapi.board.types.Track` Any — used to prove a trace
    /// segment's UUID is refused rather than silently deleted as a via, since
    /// both share KiCad's KOT_PCB_TRACE family selector.
    fn packed_track(uuid: &str) -> prost_types::Any {
        let mut track =
            konnect_ipc::builders::build_track("GND", 7, "F.Cu", 0.25, 0.0, 0.0, 1.0, 1.0);
        track.id = Some(kiapi::common::types::Kiid {
            value: uuid.to_string(),
        });
        konnect_ipc::builders::pack_any(&track, "kiapi.board.types.Track")
    }

    #[tokio::test]
    async fn trace_and_missing_uuids_refuse_before_delete_items() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        let original = b"(kicad_pcb (version 20260206))";
        std::fs::write(&board, original).unwrap();
        let delete_count = Arc::new(Mutex::new(0usize));
        let delete_count_in_mock = delete_count.clone();
        let track = packed_track("segment-1");
        let address = spawn_kicad_holding_board(&board, move |command| {
            if command.type_url.ends_with("GetItems") {
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items: vec![track.clone()],
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ));
            }
            if command.type_url.ends_with("DeleteItems") {
                *delete_count_in_mock.lock().unwrap() += 1;
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::DeleteItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        deleted_items: vec![],
                    },
                    "kiapi.common.commands.DeleteItemsResponse",
                ));
            }
            None
        });

        let ctx = ctx_talking_to(address);
        // "segment-1" belongs to a trace, not a via: must refuse, not delete.
        for uuid in ["segment-1", "missing-1"] {
            let result = handle_delete_via(
                &json!({ "board": board.to_string_lossy(), "uuid": uuid }),
                &ctx,
            )
            .await
            .unwrap();

            assert!(result.is_error, "{uuid} unexpectedly succeeded");
            assert_eq!(
                crate::mcp::error::extract_error_kind(&result).as_deref(),
                Some("stale_target"),
                "wrong error for {uuid}"
            );
        }
        assert_eq!(*delete_count.lock().unwrap(), 0);
        assert_eq!(std::fs::read(&board).unwrap(), original);
    }

    #[tokio::test]
    async fn success_reports_the_observed_via_and_verified_postcondition() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
        let deleted = Arc::new(Mutex::new(false));
        let deleted_in_mock = deleted.clone();
        let packed = packed_via("via-1", "GND", 7, 1.0, 2.0);
        let address = spawn_kicad_holding_board(&board, move |command| {
            if command.type_url.ends_with("GetItems") {
                let items = if *deleted_in_mock.lock().unwrap() {
                    vec![]
                } else {
                    vec![packed.clone()]
                };
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items,
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ));
            }
            if command.type_url.ends_with("DeleteItems") {
                *deleted_in_mock.lock().unwrap() = true;
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::DeleteItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        deleted_items: vec![],
                    },
                    "kiapi.common.commands.DeleteItemsResponse",
                ));
            }
            None
        });

        let result = handle_delete_via(
            &json!({ "board": board.to_string_lossy(), "uuid": "via-1" }),
            &ctx_talking_to(address),
        )
        .await
        .unwrap();

        assert!(!result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["deleted_uuid"], json!("via-1"));
        assert_eq!(body["deleted_type"], json!("via"));
        assert_eq!(body["preimage"]["net"], json!("GND"));
        assert_eq!(body["preimage"]["position"], json!({ "x": 1.0, "y": 2.0 }));
        assert_eq!(body["preimage"]["drill"], json!(0.4));
        assert_eq!(body["preimage"]["pad_diameter"], json!(0.8));
        assert_eq!(body["preimage"]["via_type"], json!("through"));
        assert_eq!(body["postcondition"], json!("absent_from_via_readback"));
    }
}

async fn handle_move_via(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let board_ipc = board.clone();
    let uuid_ipc = uuid.clone();
    let moved = match with_board_ipc_classified(ctx, &board, move |client| {
        client.move_via_verified(&board_ipc, &uuid_ipc, x, y)
    })
    .await?
    {
        Ok(moved) => moved,
        Err(error) => {
            return Ok(CallToolResult::error(format!(
                "KiCAD must be running with the board loaded (IPC error: {})",
                error.message()
            )))
        }
    };

    let Some(via) = moved else {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: uuid,
                reason: "the UUID is not an observed via on the requested board".to_string(),
            },
            "The requested UUID is not a via on the requested board. No board item was moved.",
        ));
    };

    Ok(CallToolResult::json(&json!({
        "moved_uuid": via.uuid,
        "postimage": {
            "uuid": via.uuid,
            "net": via.net_name,
            "position": { "x": via.position.x, "y": via.position.y },
            "drill": via.drill,
            "pad_diameter": via.pad_diameter,
            "via_type": via.via_type
        },
        "postcondition": "position_verified_by_readback"
    })))
}

#[cfg(test)]
mod move_via_tests {
    use super::*;
    use crate::tools::pcb_board::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use konnect_ipc::gen::kiapi;
    use std::sync::{Arc, Mutex};

    fn packed_via(uuid: &str, net_name: &str, net_code: i32, x: f64, y: f64) -> prost_types::Any {
        let mut via = konnect_ipc::builders::build_via(net_name, net_code, x, y, 0.4, 0.8);
        via.id = Some(kiapi::common::types::Kiid {
            value: uuid.to_string(),
        });
        konnect_ipc::builders::pack_any(&via, "kiapi.board.types.Via")
    }

    fn packed_track(uuid: &str) -> prost_types::Any {
        let mut track =
            konnect_ipc::builders::build_track("GND", 7, "F.Cu", 0.25, 0.0, 0.0, 1.0, 1.0);
        track.id = Some(kiapi::common::types::Kiid {
            value: uuid.to_string(),
        });
        konnect_ipc::builders::pack_any(&track, "kiapi.board.types.Track")
    }

    #[tokio::test]
    async fn trace_and_missing_uuids_refuse_before_update_items() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        let original = b"(kicad_pcb (version 20260206))";
        std::fs::write(&board, original).unwrap();
        let update_count = Arc::new(Mutex::new(0usize));
        let update_count_in_mock = update_count.clone();
        let track = packed_track("segment-1");
        let address = spawn_kicad_holding_board(&board, move |command| {
            if command.type_url.ends_with("GetItems") {
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items: vec![track.clone()],
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ));
            }
            if command.type_url.ends_with("UpdateItems") {
                *update_count_in_mock.lock().unwrap() += 1;
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::UpdateItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        updated_items: vec![],
                    },
                    "kiapi.common.commands.UpdateItemsResponse",
                ));
            }
            None
        });

        let ctx = ctx_talking_to(address);
        for uuid in ["segment-1", "missing-1"] {
            let result = handle_move_via(
                &json!({ "board": board.to_string_lossy(), "uuid": uuid, "x": 5.0, "y": 6.0 }),
                &ctx,
            )
            .await
            .unwrap();

            assert!(result.is_error, "{uuid} unexpectedly succeeded");
            assert_eq!(
                crate::mcp::error::extract_error_kind(&result).as_deref(),
                Some("stale_target"),
                "wrong error for {uuid}"
            );
        }
        assert_eq!(*update_count.lock().unwrap(), 0);
        assert_eq!(std::fs::read(&board).unwrap(), original);
    }

    #[tokio::test]
    async fn success_reports_the_verified_postimage() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
        let moved = Arc::new(Mutex::new(false));
        let moved_in_mock = moved.clone();
        let before = packed_via("via-1", "GND", 7, 1.0, 2.0);
        let after = packed_via("via-1", "GND", 7, 5.0, 6.0);
        let address = spawn_kicad_holding_board(&board, move |command| {
            if command.type_url.ends_with("GetItems") {
                let items = if *moved_in_mock.lock().unwrap() {
                    vec![after.clone()]
                } else {
                    vec![before.clone()]
                };
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items,
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ));
            }
            if command.type_url.ends_with("UpdateItems") {
                *moved_in_mock.lock().unwrap() = true;
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::UpdateItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        updated_items: vec![kiapi::common::commands::ItemUpdateResult {
                            status: Some(kiapi::common::commands::ItemStatus {
                                code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                                error_message: String::new(),
                            }),
                            item: None,
                        }],
                    },
                    "kiapi.common.commands.UpdateItemsResponse",
                ));
            }
            None
        });

        let result = handle_move_via(
            &json!({ "board": board.to_string_lossy(), "uuid": "via-1", "x": 5.0, "y": 6.0 }),
            &ctx_talking_to(address),
        )
        .await
        .unwrap();

        assert!(!result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["moved_uuid"], json!("via-1"));
        assert_eq!(body["postimage"]["position"], json!({ "x": 5.0, "y": 6.0 }));
        assert_eq!(
            body["postcondition"],
            json!("position_verified_by_readback")
        );
    }
}

async fn handle_query_vias(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net = args["net_name"].as_str().map(String::from);

    let vias = ipc!(ctx, args, |c| { c.get_vias(net.as_deref()) });

    let items: Vec<serde_json::Value> = vias
        .iter()
        .map(|v| {
            json!({
                "uuid": v.uuid,
                "net": v.net_name,
                "x": v.position.x, "y": v.position.y,
                "drill": v.drill,
                "pad_diameter": v.pad_diameter,
                "via_type": v.via_type
            })
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "vias": items }),
    ))
}

#[cfg(test)]
mod query_vias_tests {
    use super::*;
    use crate::tools::pcb_board::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use konnect_ipc::gen::kiapi;

    #[tokio::test]
    async fn lists_only_vias_filtered_by_net() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();

        let mut gnd_via = konnect_ipc::builders::build_via("GND", 7, 1.0, 2.0, 0.4, 0.8);
        gnd_via.id = Some(kiapi::common::types::Kiid {
            value: "via-gnd".to_string(),
        });
        let mut pwr_via = konnect_ipc::builders::build_via("3V3", 9, 3.0, 4.0, 0.3, 0.6);
        pwr_via.id = Some(kiapi::common::types::Kiid {
            value: "via-pwr".to_string(),
        });
        let mut track =
            konnect_ipc::builders::build_track("GND", 7, "F.Cu", 0.25, 0.0, 0.0, 1.0, 1.0);
        track.id = Some(kiapi::common::types::Kiid {
            value: "segment-1".to_string(),
        });

        let items = vec![
            konnect_ipc::builders::pack_any(&gnd_via, "kiapi.board.types.Via"),
            konnect_ipc::builders::pack_any(&pwr_via, "kiapi.board.types.Via"),
            konnect_ipc::builders::pack_any(&track, "kiapi.board.types.Track"),
        ];
        let address = spawn_kicad_holding_board(&board, move |command| {
            if command.type_url.ends_with("GetItems") {
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items: items.clone(),
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ));
            }
            None
        });
        let ctx = ctx_talking_to(address);

        let all = handle_query_vias(&json!({ "board": board.to_string_lossy() }), &ctx)
            .await
            .unwrap();
        assert!(!all.is_error);
        let text = match all.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            body["count"],
            json!(2),
            "trace segment leaked into vias: {body}"
        );

        let filtered = handle_query_vias(
            &json!({ "board": board.to_string_lossy(), "net_name": "3V3" }),
            &ctx,
        )
        .await
        .unwrap();
        let text = match filtered.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["count"], json!(1));
        assert_eq!(body["vias"][0]["uuid"], json!("via-pwr"));
    }
}

async fn handle_check_route_collision(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.25);

    let layer_ipc = layer.clone();
    let (tracks, vias, pads, rules) = ipc!(ctx, args, |c| {
        let tracks = c.get_tracks(None, Some(layer_ipc.as_str()))?;
        let vias = c.get_vias(None)?;
        let pads = c.get_all_pads()?;
        let rules = c.get_effective_routing_rules()?;
        Ok::<_, anyhow::Error>((tracks, vias, pads, rules))
    });

    let half_width = width / 2.0;
    let a1 = (x1, y1);
    let a2 = (x2, y2);
    let mut conflicts: Vec<serde_json::Value> = Vec::new();

    for track in &tracks {
        if track.net_name == net_name {
            continue;
        }
        let required = required_clearance_mm(&rules, &net_name, &track.net_name);
        let distance = segment_segment_distance_mm(
            a1,
            a2,
            (track.start.x, track.start.y),
            (track.end.x, track.end.y),
        );
        let found = distance - half_width - track.width / 2.0;
        if found < required {
            conflicts.push(json!({
                "kind": "track",
                "uuid": track.uuid,
                "net": track.net_name,
                "layer": track.layer,
                "clearance_required_mm": required,
                "clearance_found_mm": found
            }));
        }
    }

    for via in &vias {
        if via.net_name == net_name {
            continue;
        }
        let required = required_clearance_mm(&rules, &net_name, &via.net_name);
        let distance = point_segment_distance_mm((via.position.x, via.position.y), a1, a2);
        let found = distance - half_width - via.pad_diameter / 2.0;
        if found < required {
            conflicts.push(json!({
                "kind": "via",
                "uuid": via.uuid,
                "net": via.net_name,
                "clearance_required_mm": required,
                "clearance_found_mm": found
            }));
        }
    }

    // Component pads: the gap this tool shipped with. A proposed trace was
    // only ever checked against existing copper (tracks/vias), never against
    // the pads sitting on the layer it would cross — on a dense board this
    // let route_pad_to_pad draw straight through a neighbor's pad every time,
    // undetected, because this was the only pre-flight check available.
    // Pads are approximated as a circle of radius max(size_x, size_y)/2,
    // matching how a via's pad_diameter is already treated as a circle above
    // — conservative (never smaller than the real footprint) rather than
    // exact, which is the right tradeoff for a pre-flight refuse-if-unsure
    // check.
    for board_pad in &pads {
        let pad = &board_pad.pad;
        if pad.net == net_name {
            continue;
        }
        if pad.net.is_empty() {
            continue;
        }
        if !pad.layers.iter().any(|l| l == &layer) {
            continue;
        }
        let radius = pad.size_x.max(pad.size_y) / 2.0;
        if radius <= 0.0 {
            continue;
        }
        let required = required_clearance_mm(&rules, &net_name, &pad.net);
        let distance = point_segment_distance_mm((pad.x, pad.y), a1, a2);
        let found = distance - half_width - radius;
        if found < required {
            conflicts.push(json!({
                "kind": "pad",
                "reference": board_pad.reference,
                "pad_number": pad.number,
                "net": pad.net,
                "clearance_required_mm": required,
                "clearance_found_mm": found
            }));
        }
    }

    Ok(CallToolResult::json(&json!({
        "clear": conflicts.is_empty(),
        "checked": { "tracks": tracks.len(), "vias": vias.len(), "pads": pads.len() },
        "conflicts": conflicts
    })))
}

#[cfg(test)]
mod check_route_collision_tests {
    use super::*;
    use crate::tools::pcb_board::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use konnect_ipc::gen::kiapi;

    /// A mock KiCad holding one `GND` track from (0,0) to (10,0) on F.Cu and
    /// no nets (so `get_effective_routing_rules` falls back to the stock
    /// default clearance for every pair).
    fn mock_with_one_track(board: &std::path::Path) -> String {
        let mut track =
            konnect_ipc::builders::build_track("GND", 7, "F.Cu", 0.25, 0.0, 0.0, 10.0, 0.0);
        track.id = Some(kiapi::common::types::Kiid {
            value: "segment-1".to_string(),
        });
        let packed = konnect_ipc::builders::pack_any(&track, "kiapi.board.types.Track");
        spawn_kicad_holding_board(board, move |command| {
            if command.type_url.ends_with("GetItems") {
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items: vec![packed.clone()],
                    },
                    "kiapi.common.commands.GetItemsResponse",
                ));
            }
            if command.type_url.ends_with("GetNets") {
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::board::commands::NetsResponse { nets: vec![] },
                    "kiapi.board.commands.NetsResponse",
                ));
            }
            None
        })
    }

    #[tokio::test]
    async fn same_layer_overlapping_different_net_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
        let address = mock_with_one_track(&board);

        // A proposed 3V3 trace crossing straight through the existing GND
        // track at (5,0): must be refused.
        let result = handle_check_route_collision(
            &json!({
                "board": board.to_string_lossy(),
                "net_name": "3V3",
                "layer": "F.Cu",
                "x1": 5.0, "y1": -5.0,
                "x2": 5.0, "y2": 5.0,
                "width": 0.25
            }),
            &ctx_talking_to(address),
        )
        .await
        .unwrap();

        assert!(!result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["clear"], json!(false));
        assert_eq!(body["conflicts"].as_array().unwrap().len(), 1);
        assert_eq!(body["conflicts"][0]["kind"], json!("track"));
        assert_eq!(body["conflicts"][0]["uuid"], json!("segment-1"));
        assert_eq!(body["conflicts"][0]["net"], json!("GND"));
        // Segments cross: found clearance is deeply negative (full overlap).
        assert!(body["conflicts"][0]["clearance_found_mm"].as_f64().unwrap() < 0.0);
        assert_eq!(body["conflicts"][0]["clearance_required_mm"], json!(0.2));
    }

    #[tokio::test]
    async fn a_genuinely_clear_path_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
        let address = mock_with_one_track(&board);

        // A proposed 3V3 trace far away from the existing GND track (which
        // spans y=0 from x=0..10): must be reported clear.
        let result = handle_check_route_collision(
            &json!({
                "board": board.to_string_lossy(),
                "net_name": "3V3",
                "layer": "F.Cu",
                "x1": 0.0, "y1": 20.0,
                "x2": 10.0, "y2": 20.0,
                "width": 0.25
            }),
            &ctx_talking_to(address),
        )
        .await
        .unwrap();

        assert!(!result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["clear"], json!(true));
        assert_eq!(body["conflicts"].as_array().unwrap().len(), 0);
        assert_eq!(body["checked"]["tracks"], json!(1));
    }

    #[tokio::test]
    async fn same_net_overlap_is_not_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();
        let address = mock_with_one_track(&board);

        // Same net (GND) crossing the existing GND track: legitimately not a
        // conflict — a router may deliberately extend/overlap its own net.
        let result = handle_check_route_collision(
            &json!({
                "board": board.to_string_lossy(),
                "net_name": "GND",
                "layer": "F.Cu",
                "x1": 5.0, "y1": -5.0,
                "x2": 5.0, "y2": 5.0,
                "width": 0.25
            }),
            &ctx_talking_to(address),
        )
        .await
        .unwrap();

        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["clear"], json!(true));
    }
}

async fn handle_get_nets_list(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let nets = ipc!(ctx, args, |c| c.get_nets());
    let items: Vec<serde_json::Value> = nets
        .iter()
        .map(|n| json!({ "name": n.name, "netcode": n.netcode }))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "nets": items }),
    ))
}

/// A recognized complementary suffix pair for differential-pair detection,
/// e.g. `_DP`/`_DM` (USB_DP/USB_DM) or `_P`/`_N` (CLK_P/CLK_N). Matched
/// case-insensitively against the uppercased net name.
struct DiffPairPattern {
    pos: &'static str,
    neg: &'static str,
}

const DIFF_PAIR_PATTERNS: &[DiffPairPattern] = &[
    DiffPairPattern {
        pos: "_DP",
        neg: "_DM",
    },
    DiffPairPattern {
        pos: "_DP",
        neg: "_DN",
    },
    DiffPairPattern {
        pos: "_P",
        neg: "_N",
    },
    DiffPairPattern {
        pos: "_PLUS",
        neg: "_MINUS",
    },
    DiffPairPattern { pos: "+", neg: "-" },
];

/// Ground-net spellings recognized beyond the configured `net_prefix_ground`,
/// matched case-insensitively against the whole net name.
const GROUND_NET_VARIANTS: &[&str] = &[
    "GND", "GROUND", "AGND", "DGND", "PGND", "GNDA", "GNDD", "EARTH", "0V",
];

fn is_ground_net(name: &str, ground_prefix: &str) -> bool {
    let upper = name.to_uppercase();
    let prefix = ground_prefix.trim().to_uppercase();
    (!prefix.is_empty() && upper == prefix) || GROUND_NET_VARIANTS.contains(&upper.as_str())
}

fn is_power_net(name: &str, power_prefix: &str) -> bool {
    let prefix = power_prefix.trim().to_uppercase();
    !prefix.is_empty() && name.to_uppercase().starts_with(&prefix)
}

async fn handle_classify_nets(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let nets = ipc!(ctx, args, |c| c.get_nets());

    // Both prefixes may be overridden per-call; otherwise read from the
    // merged user+project config, the same source get_effective_config
    // reports.
    let power_prefix = if let Some(v) = args["net_prefix_power"].as_str() {
        v.to_string()
    } else {
        let config = crate::tools::config::effective_config_value(args, ctx).await;
        config["naming_conventions"]["net_prefix_power"]
            .as_str()
            .unwrap_or("VCC_")
            .to_string()
    };
    let ground_prefix = if let Some(v) = args["net_prefix_ground"].as_str() {
        v.to_string()
    } else {
        let config = crate::tools::config::effective_config_value(args, ctx).await;
        config["naming_conventions"]["net_prefix_ground"]
            .as_str()
            .unwrap_or("GND")
            .to_string()
    };

    // Named nets only: KiCad's unconnected pseudo-net carries an empty name
    // and is not a real net to classify.
    let by_upper: std::collections::HashMap<String, String> = nets
        .iter()
        .filter(|n| !n.name.is_empty())
        .map(|n| (n.name.to_uppercase(), n.name.clone()))
        .collect();

    let mut diff_pair_partner: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut pairs: Vec<serde_json::Value> = Vec::new();

    for pattern in DIFF_PAIR_PATTERNS {
        for (upper, original) in &by_upper {
            let Some(base) = upper.strip_suffix(pattern.pos) else {
                continue;
            };
            let candidate_upper = format!("{base}{}", pattern.neg);
            let Some(partner_original) = by_upper.get(&candidate_upper) else {
                continue;
            };
            diff_pair_partner.insert(original.clone(), partner_original.clone());
            diff_pair_partner.insert(partner_original.clone(), original.clone());
            pairs.push(json!({
                "positive": original,
                "negative": partner_original
            }));
        }
    }

    let items: Vec<serde_json::Value> = nets
        .iter()
        .filter(|n| !n.name.is_empty())
        .map(|n| {
            let class = if is_ground_net(&n.name, &ground_prefix) {
                "ground"
            } else if is_power_net(&n.name, &power_prefix) {
                "power"
            } else if diff_pair_partner.contains_key(&n.name) {
                "differential_pair"
            } else {
                "signal"
            };
            json!({
                "name": n.name,
                "netcode": n.netcode,
                "class": class,
                "differential_partner": diff_pair_partner.get(&n.name)
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "nets": items,
        "differential_pairs": pairs
    })))
}

#[cfg(test)]
mod classify_nets_tests {
    use super::*;
    use crate::tools::pcb_board::board_mock::{ctx_talking_to, spawn_kicad_holding_board};
    use konnect_ipc::gen::kiapi;

    fn net(name: &str, code: i32) -> kiapi::board::types::Net {
        kiapi::board::types::Net {
            code: Some(kiapi::board::types::NetCode { value: code }),
            name: name.to_string(),
        }
    }

    #[tokio::test]
    async fn classifies_power_ground_signal_and_detects_differential_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("target.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20260206))").unwrap();

        let nets = vec![
            net("GND", 1),
            net("VCC_3V3", 2),
            net("USB_DP", 3),
            net("USB_DM", 4),
            net("CLK_P", 5),
            net("CLK_N", 6),
            net("SPI_MOSI", 7),
        ];
        let address = spawn_kicad_holding_board(&board, move |command| {
            if command.type_url.ends_with("GetNets") {
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::board::commands::NetsResponse { nets: nets.clone() },
                    "kiapi.board.commands.NetsResponse",
                ));
            }
            None
        });

        // Prefixes passed explicitly so the test is hermetic — independent
        // of whatever naming_conventions a real ~/.konnect/config.json on
        // the machine running the test happens to hold.
        let result = handle_classify_nets(
            &json!({
                "board": board.to_string_lossy(),
                "net_prefix_power": "VCC_",
                "net_prefix_ground": "GND"
            }),
            &ctx_talking_to(address),
        )
        .await
        .unwrap();

        assert!(!result.is_error);
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        let class_of = |name: &str| {
            body["nets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["name"] == json!(name))
                .unwrap_or_else(|| panic!("net '{name}' missing from result: {body}"))["class"]
                .clone()
        };
        assert_eq!(class_of("GND"), json!("ground"));
        assert_eq!(class_of("VCC_3V3"), json!("power"));
        assert_eq!(class_of("USB_DP"), json!("differential_pair"));
        assert_eq!(class_of("USB_DM"), json!("differential_pair"));
        assert_eq!(class_of("CLK_P"), json!("differential_pair"));
        assert_eq!(class_of("CLK_N"), json!("differential_pair"));
        assert_eq!(class_of("SPI_MOSI"), json!("signal"));

        let pairs = body["differential_pairs"].as_array().unwrap();
        assert_eq!(
            pairs.len(),
            2,
            "expected exactly USB and CLK pairs: {pairs:?}"
        );
    }
}

async fn handle_modify_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.25);

    let uuid_ipc = uuid.clone();
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, args, |c| {
        c.delete_track(&uuid_ipc)?;
        c.add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2)
    });
    Ok(CallToolResult::json(&json!({
        "modified_uuid": uuid,
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 }
    })))
}

/// The sibling `<project>.kicad_pro`, which is where KiCad ≥ 7 keeps net
/// classes. The board file has no netclass container at all — the pre-#190
/// code inserted `(netclass …)` as a direct child of `(kicad_pcb`, a token
/// pcbnew's parser rejects, so the board no longer loaded.
fn project_settings_path(board_path: &std::path::Path) -> std::path::PathBuf {
    board_path.with_extension("kicad_pro")
}

/// Load the project JSON, refusing (rather than inventing a file KiCad never
/// reads) when it is absent.
fn load_project_settings(
    board_path: &std::path::Path,
) -> anyhow::Result<Result<(std::path::PathBuf, serde_json::Value), CallToolResult>> {
    let pro = project_settings_path(board_path);
    if !pro.exists() {
        return Ok(Err(CallToolResult::error(format!(
            "No project file at {} — net classes live in the .kicad_pro since KiCad 7, \
             and a class written anywhere else is never read. Create the project \
             (KiCad: File > Save a Copy, or place the board inside a project) and retry.",
            pro.display()
        ))));
    }
    let settings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&pro)?)
        .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", pro.display()))?;
    Ok(Ok((pro, settings)))
}

fn save_project_settings(
    pro: &std::path::Path,
    settings: &serde_json::Value,
) -> anyhow::Result<()> {
    // KiCad's own writer emits 2-space-indented JSON with alphabetical keys;
    // serde_json's pretty printer matches both, so the diff stays minimal.
    write_atomic(
        pro,
        &format!("{}\n", serde_json::to_string_pretty(settings)?),
    )?;
    Ok(())
}

/// The class every other class inherits from. `SetName` marks it on an exact
/// match (`netclass.h:96`), so casing matters.
const DEFAULT_CLASS_NAME: &str = "Default";

/// KiCad's own Default, field for field (`netclass.cpp:36-50`). Schematic
/// fields are mils, PCB fields mm.
///
/// A written Default replaces the one KiCad seeds rather than merging with it,
/// and nothing backfills it, so every key omitted here resolves from nothing.
/// Without `wire_width` that means no junction dots anywhere in the project,
/// silently (#326). See `docs/KICAD_NETCLASS_DEFAULTS.md`.
fn kicad_default_class() -> serde_json::Value {
    json!({
        // Ranks the Default last. A C++ int (`net_settings.cpp:69`), so not
        // widened past i32.
        "priority": i32::MAX,
        "schematic_color": "rgba(0, 0, 0, 0.000)",
        "pcb_color": "rgba(0, 0, 0, 0.000)",
        "tuning_profile": "",
        "wire_width": 6,
        "bus_width": 12,
        "line_style": 0,
        "clearance": 0.2,
        "track_width": 0.2,
        "via_diameter": 0.6,
        "via_drill": 0.3,
        "microvia_diameter": 0.3,
        "microvia_drill": 0.1,
        "diff_pair_width": 0.2,
        "diff_pair_gap": 0.25,
        "diff_pair_via_gap": 0.25
    })
}

async fn handle_create_netclass(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let name = match require_str(args, "name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    // KiCad's key, this tool's argument name, and the value a *new* class
    // takes when the caller says nothing. The defaults belong to creation
    // only: folding them in before an update turned "widen HV's track" into a
    // silent reset of the clearance, drill and via size the caller had tuned.
    const FIELDS: [(&str, &str, f64); 4] = [
        ("clearance", "clearance", 0.2),
        ("track_width", "trace_width", 0.25),
        ("via_drill", "via_drill", 0.4),
        ("via_diameter", "via_diameter", 0.8),
    ];
    // Only the Default must be complete. Every other class may stay sparse.
    let is_default = name == DEFAULT_CLASS_NAME;

    let (pro, mut settings) = match load_project_settings(&board_path)? {
        Ok(v) => v,
        Err(refusal) => return Ok(refusal),
    };

    let net_settings = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: top level is not a JSON object", pro.display()))?
        .entry("net_settings")
        .or_insert_with(
            || json!({ "classes": [], "meta": { "version": 5 }, "netclass_patterns": [] }),
        );
    let classes = net_settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: net_settings is not an object", pro.display()))?
        .entry("classes")
        .or_insert_with(|| json!([]));
    let classes = classes.as_array_mut().ok_or_else(|| {
        anyhow::anyhow!("{}: net_settings.classes is not an array", pro.display())
    })?;

    // KiCad keys classes by name; a second entry with the same name is
    // undefined in its dialog, so an existing class is updated in place.
    let mut changed = true;
    let mut backfilled: Vec<String> = Vec::new();
    let updated = if let Some(class) = classes.iter_mut().find(|c| c["name"] == json!(name)) {
        let before = class.clone();
        for (key, arg, _) in FIELDS {
            if let Some(value) = opt_f64(args, arg) {
                class[key] = json!(value);
            }
        }
        // KiCad backfills from the Default, never into it, so a Default left
        // incomplete by an older Konnect can only be repaired here. Absent
        // keys only: overwriting one already set is #220.
        if is_default {
            let complete = kicad_default_class();
            let seeded = complete
                .as_object()
                .expect("kicad_default_class builds an object");
            let class = class.as_object_mut().ok_or_else(|| {
                anyhow::anyhow!("{}: netclass '{name}' is not an object", pro.display())
            })?;
            for (key, value) in seeded {
                if class.get(key).is_none_or(|v| v.is_null()) {
                    class.insert(key.clone(), value.clone());
                    backfilled.push(key.clone());
                }
            }
        }
        changed = *class != before;
        true
    } else if is_default {
        // A partial write here erases KiCad's values rather than falling back
        // to them, so start from the full set.
        let mut class = kicad_default_class();
        class["name"] = json!(name);
        for (key, arg, _) in FIELDS {
            if let Some(value) = opt_f64(args, arg) {
                class[key] = json!(value);
            }
        }
        classes.push(class);
        false
    } else {
        // Sparse on purpose: a key omitted here resolves from the Default, so
        // writing the full set would sever that inheritance. -1 is what
        // KiCad's constructor gives every non-default class.
        let mut class = json!({ "name": name, "priority": -1 });
        for (key, arg, default) in FIELDS {
            class[key] = json!(opt_f64(args, arg).unwrap_or(default));
        }
        classes.push(class);
        false
    };
    // Report the class as it now stands rather than the arguments that came
    // in: on an update most of it was never named by the caller.
    let stored = classes
        .iter()
        .find(|c| c["name"] == json!(name))
        .cloned()
        .unwrap_or_else(|| json!({}));
    // Naming no value at all leaves the class exactly as it was, and so does
    // passing the values it already holds. Saving anyway would rewrite the
    // whole project file — the serialiser re-emits the document rather than
    // patching it — for a call that decided nothing.
    if changed {
        save_project_settings(&pro, &settings)?;
    }

    let note = if backfilled.is_empty() {
        "Netclasses live in the project file; assign nets with assign_net_to_class. \
         KiCad reads the change on next project open."
            .to_string()
    } else {
        format!(
            "Repaired an incomplete Default netclass by adding {}. A Default missing \
             wire_width stops Eeschema placing junctions anywhere in the project; \
             reopen the project in KiCad to pick the change up. Netclasses live in the \
             project file; assign nets with assign_net_to_class.",
            backfilled.join(", ")
        )
    };
    Ok(CallToolResult::json(&json!({
        "created_netclass": name,
        "updated_existing": updated,
        "is_default": is_default,
        "clearance": stored["clearance"], "trace_width": stored["track_width"],
        "via_drill": stored["via_drill"], "via_diameter": stored["via_diameter"],
        "file": pro.display().to_string(),
        "note": note
    })))
}

/// KiCad's netclass patterns are wildcard expressions: `*` stands for any run
/// of characters including none, `?` for exactly one. A pattern carrying
/// neither is an exact name — which is the shape `assign_net_to_class` writes.
///
/// Iterative rather than recursive: a pattern is user data, and `*`-heavy input
/// makes the naive recursion blow the stack on names a real board can carry.
fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = name.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have consumed too little.
    let (mut star, mut retry) = (None, 0usize);

    while t < txt.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            retry = t;
            p += 1;
        } else if let Some(s) = star {
            // Backtrack: let the last `*` swallow one more character.
            p = s + 1;
            retry += 1;
            t = retry;
        } else {
            return false;
        }
    }
    // Trailing `*`s may still match the empty rest.
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// The four settings `create_netclass` writes, as (KiCad's key, this API's
/// name). Reported per class so a caller can see what a class holds before
/// overwriting it — the gap that made #220 hard to review.
///
/// A key absent from a named class means inheritance, not an unset value, so
/// these are reported resolved with `inherits` naming what came from the
/// Default (#326).
const NETCLASS_FIELDS: [(&str, &str); 4] = [
    ("clearance", "clearance"),
    ("track_width", "trace_width"),
    ("via_drill", "via_drill"),
    ("via_diameter", "via_diameter"),
];

async fn handle_get_netclasses(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let (pro, settings) = match load_project_settings(&board_path)? {
        Ok(v) => v,
        Err(refusal) => return Ok(refusal),
    };

    let classes = settings["net_settings"]["classes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let patterns = settings["net_settings"]["netclass_patterns"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    // The nets come from the board file rather than IPC: this is a read-only
    // query that must answer with KiCad closed. They are read with
    // `collect_net_keys`, not `find_all("net")` — KiCad 10 writes no top-level
    // net table at all, so a direct-children scan finds zero nets on every
    // current board and every pattern would match nothing. A board held open
    // with unsaved edits reads as last saved, which the payload says out loud.
    let (net_names, nets_note): (Vec<String>, String) = match std::fs::read_to_string(&board_path) {
        Ok(text) => match konnect_sexp::parse_sexp(&text) {
            Ok(tree) => {
                let mut names: Vec<String> = konnect_sexp::net::collect_net_keys(&tree)
                    .into_iter()
                    .collect();
                names.sort();
                (
                    names,
                    "board file as last saved; unsaved edits in a running KiCad are not visible"
                        .to_string(),
                )
            }
            Err(e) => (
                Vec::new(),
                format!("board could not be parsed ({e}); nets unavailable"),
            ),
        },
        Err(e) => (
            Vec::new(),
            format!("board could not be read ({e}); nets unavailable"),
        ),
    };

    // What an omitted key resolves to. A Default in the file is the whole
    // fallback, holes included; KiCad's seeded one applies only when the file
    // has none.
    let seeded = kicad_default_class();
    let effective_default = classes
        .iter()
        .find(|c| c["name"] == json!(DEFAULT_CLASS_NAME))
        .cloned()
        .unwrap_or_else(|| seeded.clone());

    let mut out = Vec::new();
    for class in &classes {
        let name = class["name"].as_str().unwrap_or_default().to_string();
        // KiCad's own fallback class. Its clearance explains DRC results that
        // no explicit class accounts for, so it is reported, not filtered.
        let is_default = name == "Default";

        let mine: Vec<&serde_json::Value> = patterns
            .iter()
            .filter(|p| p["netclass"].as_str() == Some(name.as_str()))
            .collect();
        let pattern_strings: Vec<String> = mine
            .iter()
            .filter_map(|p| p["pattern"].as_str().map(String::from))
            .collect();

        let mut matched: Vec<String> = net_names
            .iter()
            .filter(|net| pattern_strings.iter().any(|pat| wildcard_matches(pat, net)))
            .cloned()
            .collect();
        matched.sort();
        matched.dedup();

        let mut entry = json!({
            "name": name,
            "is_default": is_default,
            "priority": class["priority"],
            "patterns": pattern_strings,
            "matched_nets": matched,
        });
        let mut inherits: Vec<&str> = Vec::new();
        for (key, api) in NETCLASS_FIELDS {
            let own = class.get(key).filter(|v| !v.is_null());
            entry[api] = match own {
                Some(value) => value.clone(),
                None if !is_default => {
                    let inherited = effective_default[key].clone();
                    if !inherited.is_null() {
                        inherits.push(api);
                    }
                    inherited
                }
                None => serde_json::Value::Null,
            };
        }
        entry["inherits"] = json!(inherits);

        // Nothing backfills the Default, so a key missing here is missing
        // project-wide — and KiCad reports none of it.
        if is_default {
            let missing: Vec<&str> = seeded
                .as_object()
                .expect("kicad_default_class builds an object")
                .keys()
                .filter(|key| class.get(*key).is_none_or(|v| v.is_null()))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                entry["note"] = json!(format!(
                    "Incomplete Default: KiCad resolves these from nothing. Repair with \
                     create_netclass(name=\"Default\")."
                ));
            }
            entry["missing_fields"] = json!(missing);
        }
        out.push(entry);
    }

    // A pattern naming a class that does not exist does nothing in KiCad, and
    // is invisible in its dialog. Surfacing it here is the whole point of a
    // read tool: it is the state assign_net_to_class refuses to create but a
    // hand-edited or third-party project file can still hold.
    let orphan_patterns: Vec<serde_json::Value> = patterns
        .iter()
        .filter(|p| {
            let target = p["netclass"].as_str().unwrap_or_default();
            !classes.iter().any(|c| c["name"] == json!(target))
        })
        .cloned()
        .collect();

    // Netclass membership is many-to-many: KiCad forms an aggregate class per
    // net, taking each property from the highest-priority class that sets it,
    // with Default filling what is left. Naming one winning class per net
    // would be a fiction, so the mapping is reported as it is.
    Ok(CallToolResult::json(&json!({
        "file": pro.display().to_string(),
        "count": out.len(),
        "netclasses": out,
        "nets_on_board": net_names.len(),
        "nets_source": nets_note,
        "orphan_patterns": orphan_patterns,
        "note": "A net can match several classes; KiCad then takes each property from the \
                 highest-priority class that sets it and falls back to Default. Lower \
                 priority numbers rank higher.",
    })))
}

async fn handle_assign_net_to_class(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let netclass = match require_str(args, "netclass") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let (pro, mut settings) = match load_project_settings(&board_path)? {
        Ok(v) => v,
        Err(refusal) => return Ok(refusal),
    };

    // The class must exist — a pattern naming an unknown class silently does
    // nothing in KiCad, which is exactly the failure shape #190 removed.
    let known: Vec<String> = settings["net_settings"]["classes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if !known.iter().any(|n| n == &netclass) {
        return Ok(CallToolResult::error(format!(
            "Netclass '{}' not found in {} — available: {}. Create it with create_netclass.",
            netclass,
            pro.display(),
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        )));
    }

    // Membership is a netclass_patterns entry; the exact net name is a valid
    // pattern. One pattern maps to one class, so a re-assignment moves the
    // entry rather than adding a competing one.
    let patterns = settings["net_settings"]
        .as_object_mut()
        .expect("checked above")
        .entry("netclass_patterns")
        .or_insert_with(|| json!([]));
    let patterns = patterns.as_array_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: net_settings.netclass_patterns is not an array",
            pro.display()
        )
    })?;

    let mut previous_class: Option<String> = None;
    if let Some(entry) = patterns
        .iter_mut()
        .find(|p| p["pattern"] == json!(net_name))
    {
        if entry["netclass"] == json!(netclass) {
            return Ok(CallToolResult::json(&json!({
                "already_assigned": true,
                "net_name": net_name,
                "netclass": netclass,
                "file": pro.display().to_string()
            })));
        }
        previous_class = entry["netclass"].as_str().map(String::from);
        entry["netclass"] = json!(netclass);
    } else {
        patterns.push(json!({ "netclass": netclass, "pattern": net_name }));
    }
    save_project_settings(&pro, &settings)?;

    Ok(CallToolResult::json(&json!({
        "assigned": true,
        "net_name": net_name,
        "netclass": netclass,
        "previous_class": previous_class,
        "file": pro.display().to_string()
    })))
}

async fn handle_route_diff_pair(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_for_gate = get_path(args, "board")?;
    if let Err(error) = require_phase_at_least(&board_for_gate, Phase::CriticalRouting).await {
        return Ok(error);
    }
    let net_pos = match require_str(args, "net_pos") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_neg = match require_str(args, "net_neg") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.1);
    let gap = args["gap"].as_f64().unwrap_or(0.1);
    let offset = (gap + width) / 2.0;

    // Route two parallel traces offset perpendicular to the direction
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    let perp_x = -dy / len * offset;
    let perp_y = dx / len * offset;

    let np_ipc = net_pos.clone();
    let nn_ipc = net_neg.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, args, |c| {
        c.add_track(
            &np_ipc,
            &layer_ipc,
            width,
            x1 + perp_x,
            y1 + perp_y,
            x2 + perp_x,
            y2 + perp_y,
        )?;
        c.add_track(
            &nn_ipc,
            &layer_ipc,
            width,
            x1 - perp_x,
            y1 - perp_y,
            x2 - perp_x,
            y2 - perp_y,
        )
    });

    Ok(CallToolResult::json(&json!({
        "net_pos": net_pos, "net_neg": net_neg,
        "layer": layer, "width": width, "gap": gap
    })))
}

/// Manual live acceptance gate for the final #337 undo boundary.
///
/// KiCad IPC can create a named commit but exposes no command that invokes the
/// editor's Undo action. This test therefore performs the complete import,
/// proves that routing appeared, and then waits for the operator to press
/// Ctrl+Z once in PCB Editor. It passes only when the exact pre-import IPC
/// snapshot returns. The UI action is test evidence; Konnect's runtime remains
/// IPC-only.
#[cfg(test)]
mod specctra_live_undo_test {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => {
                serde_json::from_str(text).expect("handler returned JSON text")
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn created_route_items_must_return_unique_kicad_ids() {
        use konnect_ipc::gen::kiapi::board::types::{Track, Via};
        use konnect_ipc::gen::kiapi::common::types::Kiid;

        let track = Track {
            id: Some(Kiid {
                value: "track-id".into(),
            }),
            ..Default::default()
        };
        let via = Via {
            id: Some(Kiid {
                value: "via-id".into(),
            }),
            ..Default::default()
        };
        let items = vec![
            konnect_ipc::builders::pack_any(&track, "kiapi.board.types.Track"),
            konnect_ipc::builders::pack_any(&via, "kiapi.board.types.Via"),
        ];
        assert_eq!(
            created_route_item_ids(&items).unwrap(),
            ["track-id", "via-id"]
        );

        let duplicate = vec![items[0].clone(), items[0].clone()];
        assert!(created_route_item_ids(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }

    #[tokio::test]
    #[ignore = "requires a disposable locked fixture open in KiCad and one manual Ctrl+Z"]
    async fn one_undo_restores_the_exact_pre_import_board_snapshot() {
        let board = std::path::PathBuf::from(
            std::env::var_os("KONNECT_LIVE_SPECCTRA_BOARD")
                .expect("set KONNECT_LIVE_SPECCTRA_BOARD to the disposable open board"),
        )
        .canonicalize()
        .expect("resolve disposable board");
        let ipc_address = std::env::var("KICAD_API_SOCKET")
            .expect("set KICAD_API_SOCKET to the PCB Editor IPC endpoint");
        let kicad_cli = std::env::var("KICAD_CLI_PATH").unwrap_or_else(|_| "kicad-cli".into());
        let freerouting_jar = std::path::PathBuf::from(
            std::env::var_os("FREEROUTING_JAR").expect("set FREEROUTING_JAR"),
        );
        let client = konnect_ipc::KiCadIpcClient::new(&ipc_address);
        let document = client.find_open_board(&board).expect("find open board");
        let before = client
            .save_document_to_string_in(document.clone())
            .expect("capture board before import");
        let rules = client
            .get_effective_routing_rules_in(document.clone())
            .expect("capture routing rules");
        let export = crate::specctra::export_dsn(&board, &before, &rules)
            .expect("export locked-routing fixture");

        let temp = tempfile::tempdir().expect("create output directory");
        let dsn = temp.path().join("board.dsn");
        let manifest = temp.path().join("board.dsn.konnect.json");
        let ses = temp.path().join("board.ses");
        let candidate = temp.path().join("board.freerouted.kicad_pcb");
        std::fs::write(&dsn, export.dsn).expect("write deterministic DSN");
        std::fs::write(&manifest, export.manifest).expect("write reverse manifest");
        crate::freerouting_mcp::route_local(
            &freerouting_jar,
            &dsn,
            &ses,
            &crate::freerouting_mcp::RouteSettings {
                max_passes: Some(20),
                optimizer_enabled: Some(false),
                job_timeout_seconds: Some(120),
                poll_interval: Duration::from_secs(2),
                overall_timeout: Duration::from_secs(180),
            },
        )
        .await
        .expect("route deterministic DSN through local Freerouting MCP");
        let ctx = ToolContext::new(
            ServerConfig {
                kicad_cli,
                kicad_binary: String::new(),
                ipc_address: ipc_address.clone(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        );
        let result = handle_apply_specctra_ses(
            &json!({
                "board": board,
                "ses_path": ses,
                "manifest_path": manifest,
                "candidate_output_path": candidate
            }),
            &ctx,
        )
        .await
        .expect("apply handler returned");
        assert!(!result.is_error, "{}", response_json(&result));
        let body = response_json(&result);
        assert_eq!(body["success"], true);
        assert_eq!(body["undo_description"], "Import Freerouting SES");
        assert!(body["created_count"].as_u64().unwrap_or(0) > 0);

        let after = client
            .save_document_to_string_in(document.clone())
            .expect("capture board after import");
        assert_ne!(after, before, "import created no observable board change");
        eprintln!("LIVE_UNDO_READY: press Ctrl+Z once in PCB Editor");

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let observed = client
                .save_document_to_string_in(document.clone())
                .expect("observe board while waiting for undo");
            if observed == before {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "one Ctrl+Z did not restore the exact pre-import IPC snapshot within 60 seconds"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod add_net_format_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// Runs add_net against a throwaway copy of `board` and returns the
    /// handler result together with the file as it stands afterwards, so a
    /// test can assert both what the caller was told and what was written.
    async fn add_net_to(board: &str) -> (CallToolResult, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board).unwrap();
        let result = handle_add_net(
            &json!({ "board": path.to_str().unwrap(), "net_name": "NEWNET" }),
            &test_ctx(),
        )
        .await
        .expect("handler should return");
        let after = std::fs::read_to_string(&path).unwrap();
        (result, after)
    }

    fn text_of(result: &CallToolResult) -> String {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        }
    }

    /// A KiCad 10 board has no net table at all, so there is nothing an insert
    /// can add. Writing one anyway is the "reports success, does nothing"
    /// pattern — the board would be unchanged in KiCad's eyes while the caller
    /// was told the net existed.
    #[tokio::test]
    async fn a_kicad_10_board_is_refused_rather_than_silently_edited() {
        let board = "(kicad_pcb\n\t(version 20260206)\n\
            \t(segment (start 0 0) (end 1 0) (net \"GND\"))\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(result.is_error, "must fail closed: {}", text_of(&result));
        let msg = text_of(&result);
        assert!(msg.contains("KiCad 10"), "{msg}");
        assert!(msg.contains("route_trace"), "must point somewhere: {msg}");
        assert_eq!(after, board, "the board must not be touched");
    }

    /// Even with no net named anywhere, the format version still identifies a
    /// KiCad 10 board, and refusing is the safe direction when structure alone
    /// cannot say.
    #[tokio::test]
    async fn a_blank_kicad_10_board_is_refused_on_its_version() {
        let board = "(kicad_pcb\n\t(version 20260306)\n\t(generator \"pcbnew\")\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(result.is_error, "{}", text_of(&result));
        assert_eq!(after, board);
    }

    #[tokio::test]
    async fn a_legacy_board_still_gets_its_net() {
        let board = "(kicad_pcb\n  (version 20241229)\n  (net 0 \"\")\n  (net 1 \"GND\")\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert!(after.contains("(net 2 \"NEWNET\")"), "{after}");
    }

    /// The old id was `content.matches("(net ").count()`, which counted every
    /// reference on every pad, segment and zone — so on any real board the
    /// "next" id collided with ids already in use.
    #[tokio::test]
    async fn the_next_id_is_one_past_the_highest_not_a_count_of_occurrences() {
        let board = "(kicad_pcb\n  (version 20241229)\n  (net 0 \"\")\n  (net 1 \"GND\")\n  \
            (net 7 \"VCC\")\n  (segment (start 0 0) (end 1 0) (net 7))\n  \
            (segment (start 1 0) (end 2 0) (net 7))\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert!(after.contains("(net 8 \"NEWNET\")"), "{after}");
    }

    /// A 9.99 development build wrote the legacy shape with a 2025 version
    /// number; treating it as KiCad 10 on the version alone would refuse a
    /// board that an insert works perfectly well on.
    #[tokio::test]
    async fn a_9_99_development_format_is_treated_as_legacy() {
        let board = "(kicad_pcb\n  (version 20250610)\n  (net 0 \"\")\n)\n";
        let (result, after) = add_net_to(board).await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert!(after.contains("(net 1 \"NEWNET\")"), "{after}");
    }
}

/// Netclasses live in `<project>.kicad_pro` since KiCad 7, not the board.
/// The old handlers inserted a `(netclass …)` node into the `.kicad_pcb` —
/// as a direct child of `(kicad_pcb` on any modern board, a token pcbnew's
/// parser rejects outright, so the board no longer loaded (#190).
#[cfg(test)]
mod netclass_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    const BOARD: &str = "(kicad_pcb\n\t(version 20250610)\n\t(generator \"pcbnew\")\n)\n";

    /// A board plus, optionally, the sibling `.kicad_pro` KiCad writes.
    fn fixture(with_project: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("demo.kicad_pcb");
        std::fs::write(&board, BOARD).unwrap();
        if with_project {
            std::fs::write(
                dir.path().join("demo.kicad_pro"),
                serde_json::to_string_pretty(&json!({
                    "board": { "design_settings": {} },
                    "meta": { "filename": "demo.kicad_pro", "version": 3 }
                }))
                .unwrap(),
            )
            .unwrap();
        }
        (dir, board)
    }

    fn text_of(r: &CallToolResult) -> String {
        match r.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn project_json(board: &std::path::Path) -> serde_json::Value {
        let pro = board.with_extension("kicad_pro");
        serde_json::from_str(&std::fs::read_to_string(pro).unwrap()).unwrap()
    }

    async fn create(board: &std::path::Path, args: serde_json::Value) -> CallToolResult {
        let mut args = args;
        args["board"] = json!(board.to_str().unwrap());
        handle_create_netclass(&args, &test_ctx()).await.unwrap()
    }

    async fn assign(board: &std::path::Path, net: &str, class: &str) -> CallToolResult {
        handle_assign_net_to_class(
            &json!({ "board": board.to_str().unwrap(), "net_name": net, "netclass": class }),
            &test_ctx(),
        )
        .await
        .unwrap()
    }

    /// The board file is data KiCad refuses if a netclass node lands in it;
    /// the class must go into the project file and the board must not change
    /// by a single byte.
    #[tokio::test]
    async fn create_netclass_writes_the_project_file_and_leaves_the_board_alone() {
        let (_dir, board) = fixture(true);
        let result = create(
            &board,
            json!({ "name": "HV", "clearance": 0.5, "trace_width": 0.3 }),
        )
        .await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);

        let pro = project_json(&board);
        let classes = pro["net_settings"]["classes"].as_array().unwrap();
        let hv = classes
            .iter()
            .find(|c| c["name"] == "HV")
            .expect("HV class in net_settings.classes");
        assert_eq!(hv["clearance"], json!(0.5));
        assert_eq!(hv["track_width"], json!(0.3));
        assert_eq!(hv["via_diameter"], json!(0.8));
        assert_eq!(hv["via_drill"], json!(0.4));
        // The existing project content survives the edit.
        assert_eq!(pro["meta"]["filename"], json!("demo.kicad_pro"));
    }

    /// #326: a Default written with only the four PCB fields leaves KiCad's
    /// schematic side resolved from nothing — Eeschema silently refuses to
    /// place a junction anywhere in the project. KiCad replaces its own seeded
    /// default with whatever the file holds and backfills nothing into it, so
    /// the file must carry the whole class.
    #[tokio::test]
    async fn create_netclass_writes_the_default_class_complete() {
        let (_dir, board) = fixture(true);
        let result = create(&board, json!({ "name": "Default" })).await;
        assert!(!result.is_error, "{}", text_of(&result));

        let pro = project_json(&board);
        let default = pro["net_settings"]["classes"][0].clone();
        assert_eq!(default["wire_width"], json!(6), "{default}");
        assert_eq!(default["bus_width"], json!(12), "{default}");
        assert_eq!(default["line_style"], json!(0), "{default}");
        // KiCad's own values, not this tool's schema defaults: a written
        // Default replaces the seeded one, so anything else silently re-specs
        // the board's routing rules.
        assert_eq!(default["track_width"], json!(0.2), "{default}");
        assert_eq!(default["via_diameter"], json!(0.6), "{default}");
        assert_eq!(default["via_drill"], json!(0.3), "{default}");
        assert_eq!(default["priority"], json!(i32::MAX), "{default}");
        for key in ["schematic_color", "pcb_color", "tuning_profile"] {
            assert!(default.get(key).is_some(), "{key} missing from {default}");
        }
    }

    /// The caller still wins over KiCad's values on the Default.
    #[tokio::test]
    async fn create_netclass_lets_the_caller_override_the_default_class() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "Default", "trace_width": 0.35 })).await;

        let default = project_json(&board)["net_settings"]["classes"][0].clone();
        assert_eq!(default["track_width"], json!(0.35), "{default}");
        assert_eq!(default["wire_width"], json!(6), "{default}");
    }

    /// The repair path. A project written by an older Konnect already holds a
    /// four-field Default, and KiCad cannot recover it: `addMissingDefaults`
    /// fills other classes *from* the Default, never the Default itself. So
    /// the update path fills what is absent — and nothing else, or this
    /// reintroduces #220.
    #[tokio::test]
    async fn create_netclass_backfills_an_incomplete_default_without_touching_set_values() {
        let (_dir, board) = fixture(true);
        let pro_path = board.with_extension("kicad_pro");
        let mut pro = project_json(&board);
        pro["net_settings"] = json!({
            "classes": [{
                "name": "Default", "priority": 0,
                "clearance": 1.5, "track_width": 0.25,
                "via_drill": 0.4, "via_diameter": 0.8
            }],
            "meta": { "version": 5 },
            "netclass_patterns": []
        });
        std::fs::write(&pro_path, serde_json::to_string_pretty(&pro).unwrap()).unwrap();

        let result = create(&board, json!({ "name": "Default" })).await;
        assert!(!result.is_error, "{}", text_of(&result));

        let default = project_json(&board)["net_settings"]["classes"][0].clone();
        assert_eq!(default["wire_width"], json!(6), "{default}");
        assert_eq!(default["bus_width"], json!(12), "{default}");
        // Everything the file already stated survives, priority included:
        // absent is repaired, present is left alone.
        assert_eq!(default["clearance"], json!(1.5), "{default}");
        assert_eq!(default["track_width"], json!(0.25), "{default}");
        assert_eq!(default["priority"], json!(0), "{default}");

        let echoed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        let note = echoed["note"].as_str().unwrap_or_default();
        assert!(note.contains("wire_width"), "{note}");
        assert!(!note.contains("clearance"), "{note}");
    }

    /// Only the Default carries the completeness requirement. A named class
    /// that omits a field inherits it, so writing the full set here would
    /// sever that inheritance and freeze the class against later edits to the
    /// Default.
    #[tokio::test]
    async fn create_netclass_leaves_a_named_class_sparse() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV", "clearance": 0.5 })).await;

        let hv = project_json(&board)["net_settings"]["classes"][0].clone();
        assert!(hv.get("wire_width").is_none(), "{hv}");
        assert!(hv.get("diff_pair_gap").is_none(), "{hv}");
        // KiCad's constructor gives every non-default class -1.
        assert_eq!(hv["priority"], json!(-1), "{hv}");
    }

    /// A project with no Default in its file keeps the one KiCad seeds at
    /// construction, which is complete. Inventing a Default alongside a named
    /// class would replace that healthy class with this tool's idea of it —
    /// the same failure as #326, wearing a different hat.
    #[tokio::test]
    async fn create_netclass_does_not_invent_a_default_alongside_a_named_class() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV" })).await;

        let classes = project_json(&board)["net_settings"]["classes"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(classes.len(), 1, "{classes:?}");
        assert_eq!(classes[0]["name"], json!("HV"), "{classes:?}");
    }

    /// The name is what makes a class the default, not its place in the array:
    /// `SetName` sets `m_isDefault` on an exact match and the loader branches
    /// on that alone. A sparse class sitting at index 0 is ordinary.
    #[tokio::test]
    async fn create_netclass_treats_the_default_by_name_not_by_position() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV" })).await;
        create(&board, json!({ "name": "Default" })).await;

        let classes = project_json(&board)["net_settings"]["classes"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(classes[0]["name"], json!("HV"), "{classes:?}");
        assert!(classes[0].get("wire_width").is_none(), "{classes:?}");
        assert_eq!(classes[1]["name"], json!("Default"), "{classes:?}");
        assert_eq!(classes[1]["wire_width"], json!(6), "{classes:?}");
    }

    /// No project file means nowhere KiCad would ever read the class from;
    /// inventing one risks orphan settings, so the tool refuses instead.
    #[tokio::test]
    async fn create_netclass_without_a_project_file_refuses_and_writes_nothing() {
        let (dir, board) = fixture(false);
        let result = create(&board, json!({ "name": "HV" })).await;
        assert!(result.is_error, "{}", text_of(&result));
        assert!(
            text_of(&result).contains("kicad_pro"),
            "{}",
            text_of(&result)
        );
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
        assert!(!dir.path().join("demo.kicad_pro").exists());
    }

    /// Same name twice updates in place — KiCad keys classes by name and two
    /// entries with one name is undefined behaviour in its dialog.
    #[tokio::test]
    async fn create_netclass_updates_an_existing_class_in_place() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV", "clearance": 0.3 })).await;
        let second = create(&board, json!({ "name": "HV", "clearance": 0.6 })).await;
        assert!(!second.is_error, "{}", text_of(&second));

        let pro = project_json(&board);
        let classes = pro["net_settings"]["classes"].as_array().unwrap();
        assert_eq!(
            classes.iter().filter(|c| c["name"] == "HV").count(),
            1,
            "{classes:?}"
        );
        assert_eq!(classes[0]["clearance"], json!(0.6));
    }

    /// Re-running the tool is how a caller adjusts one setting of a class it
    /// already tuned. Every argument carries a schema default, so applying
    /// those defaults on an update silently reset the three settings the
    /// caller did not name — the clearance a board was routed to, gone on a
    /// call that only meant to widen a track.
    #[tokio::test]
    async fn create_netclass_leaves_settings_the_caller_did_not_name_alone() {
        let (_dir, board) = fixture(true);
        create(
            &board,
            json!({ "name": "HV", "clearance": 1.5, "trace_width": 0.5,
                    "via_drill": 0.45, "via_diameter": 0.85 }),
        )
        .await;
        let second = create(&board, json!({ "name": "HV", "trace_width": 0.9 })).await;
        assert!(!second.is_error, "{}", text_of(&second));

        let pro = project_json(&board);
        let hv = pro["net_settings"]["classes"][0].clone();
        assert_eq!(hv["track_width"], json!(0.9), "the named value changes");
        assert_eq!(hv["clearance"], json!(1.5), "{hv}");
        assert_eq!(hv["via_drill"], json!(0.45), "{hv}");
        assert_eq!(hv["via_diameter"], json!(0.85), "{hv}");

        // The result echoes the stored class, not the one argument passed.
        let echoed: serde_json::Value = serde_json::from_str(&text_of(&second)).unwrap();
        assert_eq!(echoed["clearance"], json!(1.5));
        assert_eq!(echoed["trace_width"], json!(0.9));
    }

    /// With the defaults gone from the update path, a call that names no value
    /// decides nothing — so it must not write. `save_project_settings`
    /// re-serialises the whole document rather than patching it, so saving
    /// anyway rewrites every line of the project file for a call that is, in
    /// effect, a read.
    #[tokio::test]
    async fn a_call_that_changes_nothing_leaves_the_project_file_untouched() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV", "clearance": 1.5 })).await;

        // Re-written by hand in a shape the serialiser would not produce, so
        // any save at all is visible in the bytes.
        let pro = board.with_extension("kicad_pro");
        let compact = serde_json::to_string(
            &serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&pro).unwrap())
                .unwrap(),
        )
        .unwrap();
        std::fs::write(&pro, &compact).unwrap();

        // Naming no value at all: a read.
        let result = create(&board, json!({ "name": "HV" })).await;
        assert!(!result.is_error, "{}", text_of(&result));
        assert_eq!(std::fs::read_to_string(&pro).unwrap(), compact);
        // It still reports what the class holds.
        let echoed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(echoed["clearance"], json!(1.5));
        assert_eq!(echoed["updated_existing"], json!(true));

        // Naming the values it already holds: also nothing to decide.
        create(&board, json!({ "name": "HV", "clearance": 1.5 })).await;
        assert_eq!(std::fs::read_to_string(&pro).unwrap(), compact);

        // A real change still writes.
        create(&board, json!({ "name": "HV", "clearance": 0.9 })).await;
        assert_ne!(std::fs::read_to_string(&pro).unwrap(), compact);
    }

    /// A new class still gets the documented defaults for whatever the caller
    /// leaves out — the fix above must not turn creation into a partial class.
    #[tokio::test]
    async fn a_new_class_is_still_created_with_the_documented_defaults() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV" })).await;

        let hv = project_json(&board)["net_settings"]["classes"][0].clone();
        assert_eq!(hv["clearance"], json!(0.2), "{hv}");
        assert_eq!(hv["track_width"], json!(0.25), "{hv}");
        assert_eq!(hv["via_drill"], json!(0.4), "{hv}");
        assert_eq!(hv["via_diameter"], json!(0.8), "{hv}");
    }

    /// Membership is a netclass_patterns entry keyed by the exact net name.
    #[tokio::test]
    async fn assign_net_adds_a_pattern_once_and_can_move_it() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV" })).await;
        create(&board, json!({ "name": "LV" })).await;

        let first = assign(&board, "GND", "HV").await;
        assert!(!first.is_error, "{}", text_of(&first));
        let pro = project_json(&board);
        let patterns = pro["net_settings"]["netclass_patterns"].as_array().unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0]["netclass"], json!("HV"));
        assert_eq!(patterns[0]["pattern"], json!("GND"));

        // Idempotent.
        let again = assign(&board, "GND", "HV").await;
        let body: serde_json::Value = serde_json::from_str(&text_of(&again)).unwrap();
        assert_eq!(body["already_assigned"], json!(true));
        let pro = project_json(&board);
        assert_eq!(
            pro["net_settings"]["netclass_patterns"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Reassigning moves the one entry rather than adding a second.
        let moved = assign(&board, "GND", "LV").await;
        let body: serde_json::Value = serde_json::from_str(&text_of(&moved)).unwrap();
        assert_eq!(body["previous_class"], json!("HV"), "{body}");
        let pro = project_json(&board);
        let patterns = pro["net_settings"]["netclass_patterns"].as_array().unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0]["netclass"], json!("LV"));

        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }

    /// A board whose nets are named the way KiCad 10 writes them: in place, on
    /// the items themselves, with no top-level net table anywhere.
    ///
    /// This shape is the whole point of the fixture. A board carrying the
    /// pre-10 `(net 1 "GND")` table would pass even a collector that only
    /// scans direct children of `(kicad_pcb …)` — and that collector reports
    /// zero nets on every board KiCad 10 saves.
    fn fixture_with_nets(nets: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, board) = fixture(true);
        let mut text = String::from("(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n");
        text.push_str("\t(footprint \"R_0402\"\n\t\t(layer \"F.Cu\")\n");
        for (i, n) in nets.iter().enumerate() {
            text.push_str(&format!(
                "\t\t(pad \"{}\" smd roundrect\n\t\t\t(at {} 0)\n\t\t\t(net \"{}\")\n\t\t)\n",
                i + 1,
                i,
                n
            ));
        }
        text.push_str("\t)\n");
        // The same nets appear again on copper — KiCad repeats the reference on
        // every item, and a collector that counts occurrences over-reports.
        for n in nets {
            text.push_str(&format!(
                "\t(segment (start 0 0) (end 1 0) (width 0.2) (layer \"F.Cu\") (net \"{n}\"))\n"
            ));
        }
        text.push_str(")\n");
        std::fs::write(&board, text).unwrap();
        (dir, board)
    }

    async fn get_classes(board: &std::path::Path) -> serde_json::Value {
        let result =
            handle_get_netclasses(&json!({ "board": board.to_str().unwrap() }), &test_ctx())
                .await
                .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));
        serde_json::from_str(&text_of(&result)).unwrap()
    }

    #[test]
    fn wildcards_match_the_way_kicad_documents_them() {
        // An exact pattern — what assign_net_to_class writes.
        assert!(wildcard_matches("GND", "GND"));
        assert!(!wildcard_matches("GND", "GNDA"));
        // `*` spans any run, including none.
        assert!(wildcard_matches("HV_*", "HV_IN"));
        assert!(wildcard_matches("HV_*", "HV_"));
        assert!(!wildcard_matches("HV_*", "LV_IN"));
        assert!(wildcard_matches("*", "anything"));
        assert!(wildcard_matches("/Power/*", "/Power/VBUS"));
        // `?` is exactly one character.
        assert!(wildcard_matches("D?", "D0"));
        assert!(!wildcard_matches("D?", "D"));
        assert!(!wildcard_matches("D?", "D12"));
        // Backtracking: the first `*` must give characters back.
        assert!(wildcard_matches("*_P", "USB_D_P"));
        assert!(!wildcard_matches("*_P", "USB_D_N"));
        // A run of stars must not blow up or mis-answer.
        assert!(wildcard_matches("**a**", "xxaxx"));
    }

    /// The read path is the whole point of #222: what a class holds must be
    /// visible before create_netclass overwrites part of it.
    #[tokio::test]
    async fn get_netclasses_reports_settings_patterns_and_matching_nets() {
        let (_dir, board) = fixture_with_nets(&["GND", "HV_IN", "HV_OUT"]);
        create(&board, json!({ "name": "HV", "clearance": 0.5 })).await;
        assign(&board, "GND", "HV").await;

        let body = get_classes(&board).await;
        assert_eq!(body["count"], json!(1));
        let hv = &body["netclasses"][0];
        assert_eq!(hv["name"], json!("HV"));
        assert_eq!(hv["clearance"], json!(0.5));
        // Values the caller never named are reported from the file, not echoed
        // back from the arguments — the gap that made #220 unreviewable.
        assert_eq!(hv["trace_width"], json!(0.25));
        assert_eq!(hv["patterns"], json!(["GND"]));
        assert_eq!(hv["matched_nets"], json!(["GND"]));
        assert_eq!(hv["is_default"], json!(false));
        // Each net is named on a pad *and* on a segment; they are one net each.
        assert_eq!(body["nets_on_board"], json!(3));
    }

    /// The regression this tool would otherwise ship with: KiCad 10 writes no
    /// top-level net table, so reading nets as direct children of
    /// `(kicad_pcb …)` finds nothing and every pattern silently matches
    /// nothing — a plausible-looking empty answer on every current board.
    #[tokio::test]
    async fn nets_named_in_place_are_found_on_a_kicad_10_board() {
        let (_dir, board) = fixture_with_nets(&["GND", "HV_IN"]);
        // No net table at all, the way KiCad 10 saves.
        let text = std::fs::read_to_string(&board).unwrap();
        let tree = konnect_sexp::parse_sexp(&text).unwrap();
        assert_eq!(
            tree.find_all("net").len(),
            0,
            "fixture must not carry a top-level net table"
        );

        create(&board, json!({ "name": "HV" })).await;
        assign(&board, "HV_IN", "HV").await;

        let body = get_classes(&board).await;
        assert_eq!(body["nets_on_board"], json!(2));
        assert_eq!(body["netclasses"][0]["matched_nets"], json!(["HV_IN"]));
    }

    /// The pre-10 shape must keep working: there the name follows a numeric id,
    /// and the same net is repeated as a bare `(net 1)` on every item.
    #[tokio::test]
    async fn the_pre_kicad_10_net_table_still_resolves() {
        let (_dir, board) = fixture(true);
        std::fs::write(
            &board,
            "(kicad_pcb\n\t(version 20250610)\n\t(generator \"pcbnew\")\n\
             \t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(net 2 \"HV_IN\")\n\
             \t(segment (start 0 0) (end 1 0) (net 1))\n)\n",
        )
        .unwrap();
        create(&board, json!({ "name": "HV" })).await;
        assign(&board, "HV_IN", "HV").await;

        let body = get_classes(&board).await;
        // Net 0 is the unconnected pseudo-net and is not a net a user has.
        assert_eq!(body["nets_on_board"], json!(2));
        assert_eq!(body["netclasses"][0]["matched_nets"], json!(["HV_IN"]));
    }

    /// A wildcard pattern is the case a per-net lookup cannot answer: one
    /// class matches many nets, and nothing in the project file lists them.
    #[tokio::test]
    async fn a_wildcard_pattern_resolves_against_the_board_nets() {
        let (_dir, board) = fixture_with_nets(&["HV_IN", "HV_OUT", "GND"]);
        create(&board, json!({ "name": "HV" })).await;
        // assign_net_to_class only writes exact names, so write the wildcard
        // the way a user's KiCad dialog would.
        let pro = board.with_extension("kicad_pro");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        settings["net_settings"]["netclass_patterns"] =
            json!([{ "pattern": "HV_*", "netclass": "HV" }]);
        std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        assert_eq!(
            body["netclasses"][0]["matched_nets"],
            json!(["HV_IN", "HV_OUT"])
        );
    }

    /// A net may sit in several classes at once — KiCad aggregates them by
    /// priority. Reporting a single winning class per net would be a fiction.
    #[tokio::test]
    async fn one_net_can_belong_to_several_classes() {
        let (_dir, board) = fixture_with_nets(&["USB_DP"]);
        create(&board, json!({ "name": "HighSpeed" })).await;
        create(&board, json!({ "name": "Wide" })).await;
        let pro = board.with_extension("kicad_pro");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        settings["net_settings"]["netclass_patterns"] = json!([
            { "pattern": "USB_*", "netclass": "HighSpeed" },
            { "pattern": "*_DP",  "netclass": "Wide" },
        ]);
        std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        for class in body["netclasses"].as_array().unwrap() {
            assert_eq!(
                class["matched_nets"],
                json!(["USB_DP"]),
                "both classes claim the net: {class}"
            );
        }
    }

    /// A key absent from a named class is inheritance, not an unset value:
    /// KiCad fills it from the Default at load. Reporting the raw absence as
    /// null told a caller a class had no clearance when it had the Default's,
    /// which is exactly what this tool exists to prevent (#326).
    #[tokio::test]
    async fn get_netclasses_reports_inherited_settings_as_inherited() {
        let (_dir, board) = fixture(true);
        let pro_path = board.with_extension("kicad_pro");
        let mut settings = project_json(&board);
        settings["net_settings"] = json!({
            "classes": [
                { "name": "Default", "clearance": 0.3, "track_width": 0.2,
                  "via_diameter": 0.6, "via_drill": 0.3, "wire_width": 6 },
                { "name": "HV", "clearance": 0.9 }
            ],
            "netclass_patterns": []
        });
        std::fs::write(&pro_path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        let hv = body["netclasses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "HV")
            .unwrap()
            .clone();
        assert_eq!(hv["clearance"], json!(0.9), "its own value: {hv}");
        assert_eq!(hv["trace_width"], json!(0.2), "resolved from Default: {hv}");
        assert_eq!(
            hv["inherits"],
            json!(["trace_width", "via_drill", "via_diameter"]),
            "{hv}"
        );
    }

    /// The Default is the root of that inheritance and nothing backfills it,
    /// so a key missing here is missing project-wide, with no ERC violation
    /// and no visible error in KiCad. Naming it is the only way a caller finds
    /// out before the junction dots stop working.
    #[tokio::test]
    async fn get_netclasses_names_what_an_incomplete_default_cannot_resolve() {
        let (_dir, board) = fixture(true);
        let pro_path = board.with_extension("kicad_pro");
        let mut settings = project_json(&board);
        settings["net_settings"] = json!({
            "classes": [{ "name": "Default", "clearance": 0.2, "track_width": 0.25,
                          "via_drill": 0.4, "via_diameter": 0.8 }],
            "netclass_patterns": []
        });
        std::fs::write(&pro_path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        let default = body["netclasses"][0].clone();
        let missing = default["missing_fields"].as_array().unwrap();
        assert!(missing.contains(&json!("wire_width")), "{default}");
        assert!(!missing.contains(&json!("clearance")), "{default}");
        assert!(
            default["note"]
                .as_str()
                .unwrap_or_default()
                .contains("Default"),
            "{default}"
        );
    }

    /// A pattern naming a class that does not exist does nothing in KiCad and
    /// is invisible in its dialog. It is exactly what a read tool is for.
    #[tokio::test]
    async fn a_pattern_naming_no_class_is_reported_as_an_orphan() {
        let (_dir, board) = fixture_with_nets(&["GND"]);
        create(&board, json!({ "name": "HV" })).await;
        let pro = board.with_extension("kicad_pro");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        settings["net_settings"]["netclass_patterns"] =
            json!([{ "pattern": "GND", "netclass": "Vanished" }]);
        std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        assert_eq!(body["orphan_patterns"][0]["netclass"], json!("Vanished"));
        assert_eq!(body["netclasses"][0]["matched_nets"], json!([]));
    }

    /// Default is KiCad's fallback and its clearance explains DRC results no
    /// explicit class accounts for, so it is reported and marked.
    #[tokio::test]
    async fn the_default_class_is_reported_and_marked() {
        let (_dir, board) = fixture_with_nets(&["GND"]);
        let pro = board.with_extension("kicad_pro");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        settings["net_settings"] = json!({
            "classes": [{ "name": "Default", "clearance": 0.2, "track_width": 0.25,
                          "priority": i32::MAX }],
            "netclass_patterns": []
        });
        std::fs::write(&pro, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let body = get_classes(&board).await;
        assert_eq!(body["netclasses"][0]["is_default"], json!(true));
        assert_eq!(body["netclasses"][0]["priority"], json!(i32::MAX));
    }

    /// Reading must not write. The project file is byte-identical afterwards —
    /// #220's second half was create_netclass rewriting it for a no-op call.
    #[tokio::test]
    async fn reading_leaves_both_files_untouched() {
        let (_dir, board) = fixture_with_nets(&["GND"]);
        create(&board, json!({ "name": "HV" })).await;
        let pro = board.with_extension("kicad_pro");
        let before_pro = std::fs::read_to_string(&pro).unwrap();
        let before_board = std::fs::read_to_string(&board).unwrap();

        get_classes(&board).await;

        assert_eq!(std::fs::read_to_string(&pro).unwrap(), before_pro);
        assert_eq!(std::fs::read_to_string(&board).unwrap(), before_board);
    }

    /// Without a project file there are no netclasses to read, and the refusal
    /// must say where they actually live.
    #[tokio::test]
    async fn reading_without_a_project_file_refuses_with_the_reason() {
        let (_dir, board) = fixture(false);
        let result =
            handle_get_netclasses(&json!({ "board": board.to_str().unwrap() }), &test_ctx())
                .await
                .unwrap();
        assert!(result.is_error);
        assert!(
            text_of(&result).contains("kicad_pro"),
            "{}",
            text_of(&result)
        );
    }

    /// A project with no net_settings at all is a normal new project, not an
    /// error — it simply has no classes yet.
    #[tokio::test]
    async fn a_project_without_net_settings_reads_as_empty() {
        let (_dir, board) = fixture_with_nets(&["GND"]);
        let body = get_classes(&board).await;
        assert_eq!(body["count"], json!(0));
        assert_eq!(body["netclasses"], json!([]));
        assert_eq!(body["nets_on_board"], json!(1));
    }

    /// Assigning to a class that doesn't exist names the ones that do.
    #[tokio::test]
    async fn assign_net_to_a_missing_class_errors_naming_the_available_ones() {
        let (_dir, board) = fixture(true);
        create(&board, json!({ "name": "HV" })).await;
        let result = assign(&board, "GND", "NOPE").await;
        assert!(result.is_error);
        let msg = text_of(&result);
        assert!(msg.contains("HV"), "{msg}");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }
}

/// Zones must reference their net in the same shape the board uses — KiCad 10
/// by name, legacy by declared id. Both `add_copper_pour` here and `add_zone`
/// in `pcb_board.rs` used a string-offset id lookup that returned 0 on every
/// KiCad 10 board, silently attaching the pour to the unconnected pseudo-net
/// (#192).
#[cfg(test)]
mod zone_net_format_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn text_of(r: &CallToolResult) -> String {
        match r.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// KiCad 10 names nets on copper; there is no table and no ids.
    const KICAD_10: &str = "(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n\t(segment\n\t\t(start 10 10)\n\t\t(end 20 10)\n\t\t(width 0.2)\n\t\t(layer \"F.Cu\")\n\t\t(net \"GND\")\n\t)\n)\n";
    /// Legacy: table at top level, items reference by id.
    const LEGACY: &str = "(kicad_pcb\n  (version 20240108)\n  (generator \"pcbnew\")\n  (net 0 \"\")\n  (net 7 \"GND\")\n  (segment (start 10 10) (end 20 10) (width 0.2) (layer \"F.Cu\") (net 7))\n)\n";

    async fn pour(board: &str, net: &str) -> (CallToolResult, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, board).unwrap();
        let result = handle_add_copper_pour(
            &json!({
                "board": path.to_str().unwrap(), "net_name": net, "layer": "F.Cu",
                "points": [ {"x": 0.0, "y": 0.0}, {"x": 10.0, "y": 0.0}, {"x": 10.0, "y": 10.0} ]
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        (result, std::fs::read_to_string(&path).unwrap())
    }

    #[tokio::test]
    async fn a_kicad_10_zone_references_the_net_by_name() {
        let (result, after) = pour(KICAD_10, "GND").await;
        assert!(!result.is_error, "{}", text_of(&result));
        let zone_at = after.find("(zone").expect("zone written");
        let zone = &after[zone_at..];
        assert!(zone.contains("(net \"GND\")"), "{zone}");
        assert!(!zone.contains("(net 0)"), "{zone}");
        assert!(
            !zone.contains("net_name"),
            "no net_name token in KiCad 10: {zone}"
        );
        assert!(zone.contains("(layers \"F.Cu\")"), "plural layers: {zone}");
        // The #142 read helpers must see the pour on GND, not orphaned.
        let tree = konnect_sexp::parse_sexp(&after).unwrap();
        assert!(konnect_sexp::net::collect_net_keys(&tree).contains("GND"));
    }

    #[tokio::test]
    async fn a_legacy_zone_keeps_the_declared_id_and_net_name_pair() {
        let (result, after) = pour(LEGACY, "GND").await;
        assert!(!result.is_error, "{}", text_of(&result));
        let zone_at = after.find("(zone").expect("zone written");
        let zone = &after[zone_at..];
        assert!(zone.contains("(net 7) (net_name \"GND\")"), "{zone}");
        assert!(zone.contains("(layer \"F.Cu\")"), "singular layer: {zone}");
        assert!(konnect_sexp::parse_sexp(&after).is_ok());
    }

    /// The old lookup fell back to 0 — the orphan. An unknown net on a legacy
    /// board must refuse and leave the file alone.
    #[tokio::test]
    async fn an_undeclared_net_on_a_legacy_board_is_refused_not_zeroed() {
        let (result, after) = pour(LEGACY, "PWR").await;
        assert!(result.is_error, "{}", text_of(&result));
        assert!(text_of(&result).contains("add_net"), "{}", text_of(&result));
        assert_eq!(after, LEGACY, "file must be untouched");
    }

    /// `add_copper_pour` is `add_zone` under an older name. They were two
    /// near-identical copies that had already drifted — `min_width` defaulted
    /// to 0.25 here and 0.2 there, and the #192 net-lookup bug had to be fixed
    /// in both — so the alias is asserted to carry the shared defaults.
    #[tokio::test]
    async fn the_alias_carries_add_zone_s_defaults_and_fallback_warning() {
        let (result, after) = pour(KICAD_10, "GND").await;
        assert!(!result.is_error, "{}", text_of(&result));

        let zone = &after[after.find("(zone").expect("zone written")..];
        assert!(
            zone.contains("(min_thickness 0.2)"),
            "the alias used to default min_width to 0.25: {zone}"
        );
        assert!(zone.contains("(connect_pads (clearance 0.2))"), "{zone}");

        let body: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(body["source"], json!("file"));
        assert!(body["warning"]
            .as_str()
            .is_some_and(|w| w.contains("current Konnect server session")));
    }

    /// The new arguments reach the alias too — it is the same handler, and a
    /// schema that advertised them on only one of the two would be a lie.
    #[tokio::test]
    async fn the_alias_accepts_the_new_zone_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.kicad_pcb");
        std::fs::write(&path, KICAD_10).unwrap();
        let result = handle_add_copper_pour(
            &json!({
                "board": path.to_str().unwrap(), "net_name": "GND", "layer": "F.Cu",
                "points": [ {"x": 0.0, "y": 0.0}, {"x": 10.0, "y": 0.0}, {"x": 10.0, "y": 10.0} ],
                "name": "pour", "priority": 1, "pad_connection": "none"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", text_of(&result));

        let after = std::fs::read_to_string(&path).unwrap();
        let zone = &after[after.find("(zone").expect("zone written")..];
        assert!(zone.contains("(name \"pour\")"), "{zone}");
        assert!(zone.contains("(priority 1)"), "{zone}");
        assert!(zone.contains("(connect_pads no (clearance"), "{zone}");
    }
}
