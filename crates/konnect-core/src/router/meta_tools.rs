//! The 6 always-visible meta-tools.
//!
//! Discovery / routing:
//!   list_toolboxes()          — show all 17 toolsets with descriptions and load state
//!   load_toolset(name)        — activate a toolset, expose its tools in tools/list
//!   unload_toolset(name)      — deactivate a toolset, remove its tools from tools/list
//!   get_active_toolsets()     — list currently loaded toolsets
//!
//! Observability:
//!   get_recent_calls(limit?)  — last N tool calls (newest first) with timing + status
//!   server_stats()            — uptime, per-tool totals/errors, JSONL log path
//!
//! At server startup only the STARTER_KIT (`project`, `config`, `pcb_routing`)
//! is pre-loaded so baseline context stays small. The LLM reads `list_toolboxes`
//! and calls `load_toolset(name)` to expose the tools it actually needs for the
//! task. (`pcb_routing` is pre-loaded rather than on-demand because some MCP
//! clients never act on the `list_changed` notification sent after a runtime
//! `load_toolset` — see registry.rs::STARTER_KIT for details.)

use crate::mcp::protocol::{CallToolResult, McpToolDescription};
use crate::tools::ToolContext;
use serde_json::{json, Value};

/// Return the 4 meta-tool MCP descriptions (always in the tools/list response).
pub fn meta_tool_descriptions() -> Vec<McpToolDescription> {
    vec![
        McpToolDescription {
            name: "list_toolboxes".to_string(),
            description:
                "List all available KiCAD toolsets with descriptions, categories, tool counts, \
                 and whether each is currently loaded. Only the starter kit (project, config, \
                 pcb_routing) is loaded at startup — call load_toolset(name) to expose \
                 additional tools in subsequent tools/list responses. Always call this first \
                 to discover what tools are available for the task."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        McpToolDescription {
            name: "load_toolset".to_string(),
            description:
                "Load a toolset by name so its tools appear in tools/list and can be called. \
                 Returns the list of tools that were added. Use list_toolboxes() first to \
                 see valid names."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Toolset name (e.g. 'sch_components', 'pcb_routing')"
                    }
                },
                "required": ["name"]
            }),
        },
        McpToolDescription {
            name: "unload_toolset".to_string(),
            description: "Unload a toolset to remove its tools from the active session. \
                 Use this to keep the tool list manageable when switching tasks."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Toolset name to unload"
                    }
                },
                "required": ["name"]
            }),
        },
        McpToolDescription {
            name: "get_active_toolsets".to_string(),
            description:
                "Return the list of currently loaded toolsets and how many tools each provides."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        McpToolDescription {
            name: "get_recent_calls".to_string(),
            description:
                "Return the most recent tool calls this session (newest first) with call_id, \
                 tool name, toolset, duration, status (ok/error/not_found), and \
                 error_kind when failed. Use this to self-diagnose — e.g. 'why did the last call \
                 fail?' or 'what tools have I been running?'"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Max number of calls to return (default 20, max 100). Pass 0 for all buffered calls.",
                        "default": 20
                    }
                },
                "required": []
            }),
        },
        McpToolDescription {
            name: "server_stats".to_string(),
            description:
                "Return server uptime, total/error call counts, per-tool statistics, and the \
                 path to the JSONL call log. Good for 'what's my error rate today?' and \
                 'which tool has been slowest?'."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    ]
}

/// Attempt to handle a meta-tool call. Returns `None` if the name is not a meta-tool.
pub async fn handle_meta_tool(
    name: &str,
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
) -> Option<CallToolResult> {
    match name {
        "list_toolboxes" => Some(handle_list_toolboxes(ctx).await),
        "load_toolset" => Some(handle_load_toolset(args, ctx).await),
        "unload_toolset" => Some(handle_unload_toolset(args, ctx).await),
        "get_active_toolsets" => Some(handle_get_active_toolsets(ctx).await),
        "get_recent_calls" => Some(handle_get_recent_calls(args, ctx).await),
        "server_stats" => Some(handle_server_stats(ctx).await),
        _ => None,
    }
}

async fn handle_list_toolboxes(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    use std::collections::HashSet;
    let active: HashSet<String> = ctx.router.active_names().await.into_iter().collect();

    let toolsets: Vec<Value> = ctx
        .router
        .all_toolsets()
        .iter()
        .map(|t| {
            let loaded = active.contains(t.name);
            json!({
                "name": t.name,
                "description": t.description,
                "category": t.category,
                "tool_count": t.tool_count,
                "loaded": loaded,
            })
        })
        .collect();

    CallToolResult::json(&json!({
        "toolsets": toolsets,
        "total_tools": toolsets.iter()
            .filter_map(|t| t["tool_count"].as_u64())
            .sum::<u64>(),
        "loaded_count": active.len(),
        "hint": "Only loaded toolsets contribute tools to tools/list. Call load_toolset(name) \
                 to expose a toolset's tools. Call unload_toolset(name) to prune tools you no \
                 longer need (keeps context small).",
    }))
}

async fn handle_load_toolset(args: &Value, ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return CallToolResult::error("Missing required argument: name"),
    };

    match ctx.router.load(name).await {
        Some(tools) => {
            let tool_list: Vec<Value> = tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": t.description }))
                .collect();
            CallToolResult::json(&json!({
                "loaded": name,
                "tools_added": tools.len(),
                "tools": tool_list
            }))
        }
        None => CallToolResult::error(format!(
            "Unknown toolset '{}'. Call list_toolboxes() to see valid names.",
            name
        )),
    }
}

async fn handle_unload_toolset(args: &Value, ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return CallToolResult::error("Missing required argument: name"),
    };

    if ctx.router.unload(name).await {
        CallToolResult::text(format!("Toolset '{}' unloaded.", name))
    } else {
        CallToolResult::error(format!("Unknown toolset '{}'.", name))
    }
}

async fn handle_get_recent_calls(
    args: &Value,
    ctx: &std::sync::Arc<ToolContext>,
) -> CallToolResult {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(20);
    let records = ctx.observer.recent(limit).await;
    let count = records.len();
    CallToolResult::json(&json!({
        "count": count,
        "limit_applied": if limit == 0 { count } else { limit },
        "calls": records,
        "hint": "Calls are ordered newest-first. Use server_stats for aggregates.",
    }))
}

async fn handle_server_stats(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let snap = ctx.observer.snapshot().await;
    CallToolResult::json(&snap)
}

async fn handle_get_active_toolsets(ctx: &std::sync::Arc<ToolContext>) -> CallToolResult {
    let active = ctx.router.active_names().await;
    let all = ctx.router.all_toolsets();

    let result: Vec<Value> = active
        .iter()
        .filter_map(|name| {
            all.iter().find(|t| t.name == name.as_str()).map(|meta| {
                json!({
                    "name": meta.name,
                    "description": meta.description,
                    "tool_count": meta.tool_count
                })
            })
        })
        .collect();

    CallToolResult::json(&json!({
        "active_toolsets": result,
        "total_active_tools": result.iter()
            .filter_map(|t| t["tool_count"].as_u64())
            .sum::<u64>()
    }))
}
