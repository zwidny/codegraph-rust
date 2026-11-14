// ABOUTME: End-to-end tests for the CodeGraph MCP stdio server tool surface.
// ABOUTME: Validates tool behavior via an rmcp client against a real server process.
/// E2E Tests for CodeGraph MCP Server Tools
///
/// Tests the consolidated direct (LLM-free) MCP tools against the indexed
/// codebase using the official rmcp client library for authentic protocol
/// testing.
use anyhow::{Result, anyhow};
use rmcp::{RoleClient, ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command as TokioCommand;

static CODEGRAPH_BIN: OnceLock<PathBuf> = OnceLock::new();

fn workspace_root_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("Expected CARGO_MANIFEST_DIR to be under <workspace>/crates/<crate>")
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root_dir().join("target"))
}

fn codegraph_bin_path() -> PathBuf {
    let exe = if cfg!(windows) {
        "codegraph.exe"
    } else {
        "codegraph"
    };
    target_dir().join("fast-dev").join(exe)
}

fn ensure_codegraph_bin() -> PathBuf {
    CODEGRAPH_BIN
        .get_or_init(|| {
            let workspace_root = workspace_root_dir();
            let output = StdCommand::new("cargo")
                .current_dir(&workspace_root)
                .args([
                    "build",
                    "--profile",
                    "fast-dev",
                    "-q",
                    "-p",
                    "codegraph-mcp-server",
                    "--bin",
                    "codegraph",
                ])
                .stdin(Stdio::null())
                .output()
                .expect("Failed to invoke cargo build for codegraph-mcp-server");

            if !output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                panic!(
                    "Failed to build codegraph-mcp-server binary.\nstdout:\n{}\nstderr:\n{}",
                    stdout, stderr
                );
            }

            let bin = codegraph_bin_path();
            assert!(
                bin.exists(),
                "Expected built binary at '{}', but it does not exist",
                bin.display()
            );
            bin
        })
        .clone()
}

/// Start CodeGraph MCP server as a child process for testing
async fn start_mcp_server() -> Result<rmcp::service::RunningService<RoleClient, ()>> {
    let codegraph_bin = ensure_codegraph_bin();

    let mut cmd = TokioCommand::new(codegraph_bin);
    cmd.args(["start", "stdio"])
        .current_dir(workspace_root_dir()) // project_id and .env resolve from the indexed root
        .env("RUST_LOG", "error") // Minimize log noise during testing
        .env("CODEGRAPH_DEBUG", "0"); // Force-disable debug log files in tests

    let (transport, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::null())
        .spawn()?;

    let service = ().serve(transport).await?;
    Ok(service)
}

/// Call a direct graph tool and decode its result as JSON.
async fn call_graph_tool(
    service: &rmcp::service::RunningService<RoleClient, ()>,
    name: &str,
    args: Value,
) -> Result<Value> {
    let arguments = args
        .as_object()
        .ok_or_else(|| anyhow!("Expected object arguments for '{name}'"))?
        .clone();

    let result = service
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(arguments))
        .await?;

    let raw = serde_json::to_value(&result)?;
    let text = raw["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow!("'{name}' returned no text content"))?;
    serde_json::from_str(text).map_err(|error| anyhow!("'{name}' returned non-JSON: {error}"))
}

/// Find the first `node_id` in a semantic search response.
fn first_node_id(search: &Value) -> Option<String> {
    search["result"]
        .as_array()?
        .iter()
        .find_map(|row| row["node_id"].as_str().map(str::to_string))
}

#[tokio::test]
async fn test_tool_discovery() -> Result<()> {
    println!("🔍 Testing MCP tool discovery...");

    let service = start_mcp_server().await?;

    // The tool surface is the direct graph tools plus offline plan validation.
    // The four agentic tools (context/impact/architecture/quality) were removed
    // together with the `codegraph agent` CLI and the codegraph-mcp-rig crate.
    let expected_tools = vec![
        "semantic_code_search",
        "get_reverse_dependencies",
        "get_transitive_dependencies",
        "trace_call_chain",
        "calculate_coupling_metrics",
        "detect_circular_dependencies",
        "find_complexity_hotspots",
        "get_hub_nodes",
        // Draft logic graphs: validate reasoning before code exists (no DB).
        "plan_check",
    ];

    let tools = service.list_tools(Default::default()).await?;
    let discovered: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    println!("📋 Discovered {} tools", discovered.len());

    for expected_tool in &expected_tools {
        assert!(
            discovered.contains(expected_tool),
            "Tool '{}' not found in discovered tools: {discovered:?}",
            expected_tool
        );
        println!("✅ Tool '{}' discovered", expected_tool);
    }

    // Verify no removed agentic tool survived, and nothing else is present.
    for removed in [
        "agentic_context",
        "agentic_impact",
        "agentic_architecture",
        "agentic_quality",
    ] {
        assert!(
            !discovered.contains(&removed),
            "Tool '{removed}' should have been deleted but is still discovered: {discovered:?}"
        );
    }

    for tool in &tools.tools {
        let tool_name: &str = tool.name.as_ref();
        if !expected_tools.contains(&tool_name) {
            panic!("❌ Unexpected tool found: {}", tool.name);
        }
    }

    assert_eq!(
        tools.tools.len(),
        expected_tools.len(),
        "MCP tool surface must expose exactly {} tools: {discovered:?}",
        expected_tools.len()
    );

    service.cancel().await?;
    println!("🎉 Tool discovery test passed!");
    Ok(())
}

#[tokio::test]
async fn test_plan_check_tool_validates_draft_graph() -> Result<()> {
    let service = start_mcp_server().await?;

    let plan = json!({
        "nodes": [{ "id": "a" }, { "id": "b" }],
        "edges": [
            { "from": "a", "to": "b" },
            { "from": "b", "to": "a" }
        ],
        "constraints": [{ "kind": "acyclic" }]
    });

    let arguments = json!({ "plan": plan })
        .as_object()
        .expect("object arguments")
        .clone();
    let result = service
        .call_tool(CallToolRequestParams::new("plan_check").with_arguments(arguments))
        .await?;

    let raw = serde_json::to_value(&result)?;
    let text = raw["content"][0]["text"]
        .as_str()
        .expect("plan_check returns text content");
    let report: Value = serde_json::from_str(text)?;

    assert_eq!(report["ok"], false, "cycle must fail the check");
    assert_eq!(report["violations"][0]["rule"], "acyclic");
    assert!(
        report["checks"]
            .as_array()
            .is_some_and(|checks| checks.iter().any(|check| check["rule"] == "known_nodes")),
        "always-on known_nodes check must be present"
    );

    service.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn test_workflow_integration() -> Result<()> {
    println!("🔗 Testing direct tool workflow integration...");

    let service = start_mcp_server().await?;

    // Step 1: find a concrete node via semantic search.
    println!("🧪 Step 1: semantic_code_search");
    let search = call_graph_tool(
        &service,
        "semantic_code_search",
        json!({"query": "CodeGraphMCPServer", "limit": 5, "threshold": 0.0}),
    )
    .await?;
    assert_eq!(search["tool"], "semantic_code_search");
    let node_id = first_node_id(&search).ok_or_else(|| {
        anyhow!("semantic_code_search returned no node_id; is the project indexed?")
    })?;

    // Step 2: who depends on that node.
    println!("🧪 Step 2: get_reverse_dependencies");
    let reverse = call_graph_tool(
        &service,
        "get_reverse_dependencies",
        json!({"node_id": node_id, "edge_type": "calls", "depth": 1}),
    )
    .await?;
    assert_eq!(reverse["tool"], "get_reverse_dependencies");
    assert!(reverse["result"].is_array());

    // Step 3: follow the call chain from the same node.
    println!("🧪 Step 3: trace_call_chain");
    let chain = call_graph_tool(
        &service,
        "trace_call_chain",
        json!({"node_id": node_id, "max_depth": 2}),
    )
    .await?;
    assert_eq!(chain["tool"], "trace_call_chain");

    service.cancel().await?;
    println!("🎉 Workflow integration test passed!");
    Ok(())
}

#[tokio::test]
async fn test_error_conditions() -> Result<()> {
    println!("⚠️ Testing error condition handling...");

    let service = start_mcp_server().await?;

    // Test 1: missing required node_id is a hard error (not recoverable).
    println!("🧪 Testing missing required parameter...");
    let result = service
        .call_tool(
            CallToolRequestParams::new("get_reverse_dependencies").with_arguments(
                json!({
                    // Missing required "node_id"
                    "edge_type": "calls"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert!(
        result.is_err(),
        "Missing required 'node_id' must return an error"
    );
    println!("✅ Missing required parameter rejected");

    // Test 2: wrong parameter type is also a hard error.
    println!("🧪 Testing invalid parameter type...");
    let result = service
        .call_tool(
            CallToolRequestParams::new("get_reverse_dependencies").with_arguments(
                json!({ "node_id": 42, "edge_type": "calls" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(result.is_err(), "Non-string 'node_id' must return an error");
    println!("✅ Invalid parameter type rejected");

    // Test 3: non-existent tool.
    println!("🧪 Testing non-existent tool...");
    let result = service
        .call_tool(CallToolRequestParams::new("agentic_context"))
        .await;
    assert!(result.is_err(), "Removed agentic tool must not be callable");
    println!("✅ Properly rejected non-existent tool");

    service.cancel().await?;
    println!("🎉 Error condition test passed!");
    Ok(())
}

#[tokio::test]
async fn test_comprehensive_tool_suite() -> Result<()> {
    println!("🎯 Testing the direct tool surface comprehensively...");

    let service = start_mcp_server().await?;

    // All graph tools that take a node_id share the one discovered here.
    let search = call_graph_tool(
        &service,
        "semantic_code_search",
        json!({"query": "GraphToolExecutor", "limit": 5, "threshold": 0.0}),
    )
    .await?;
    let node_id = first_node_id(&search).ok_or_else(|| {
        anyhow!("semantic_code_search returned no node_id; is the project indexed?")
    })?;

    let tool_tests = vec![
        (
            "semantic_code_search",
            json!({"query": "tool registration", "limit": 1, "threshold": 0.0}),
        ),
        (
            "get_reverse_dependencies",
            json!({"node_id": node_id, "edge_type": "calls", "depth": 1}),
        ),
        (
            "get_transitive_dependencies",
            json!({"node_id": node_id, "edge_type": "calls", "depth": 1}),
        ),
        (
            "trace_call_chain",
            json!({"node_id": node_id, "max_depth": 1}),
        ),
        ("calculate_coupling_metrics", json!({"node_id": node_id})),
        (
            "detect_circular_dependencies",
            json!({"edge_type": "calls"}),
        ),
        ("find_complexity_hotspots", json!({"limit": 5})),
        ("get_hub_nodes", json!({"min_degree": 2})),
        (
            "plan_check",
            json!({"plan": {"nodes": [{"id": "a"}], "edges": []}}),
        ),
    ];

    for (tool_name, params) in tool_tests {
        println!("🧪 Testing tool: {}", tool_name);
        let result = call_graph_tool(&service, tool_name, params).await?;
        assert!(
            result.is_object() || result.is_array(),
            "'{tool_name}' must return a JSON object or array, got: {result}"
        );
    }

    service.cancel().await?;
    println!("🎉 Comprehensive tool surface test passed!");
    Ok(())
}

/// Helper function to verify the server is responding
#[tokio::test]
async fn test_server_health() -> Result<()> {
    println!("🏥 Testing MCP server health and responsiveness...");

    let service = start_mcp_server().await?;

    // Test basic connectivity
    let tools = service.list_tools(Default::default()).await?;
    assert!(!tools.tools.is_empty(), "Server should expose tools");

    // Test server info
    let info = service
        .peer_info()
        .expect("Server should provide peer info after initialization");
    println!(
        "📋 Server info: {:?}",
        info.server_info
            .as_ref()
            .map(|server| server.name.as_str())
            .unwrap_or("unknown")
    );

    service.cancel().await?;
    println!("🎉 Server health test passed!");
    Ok(())
}

/// Performance benchmark test
#[tokio::test]
async fn test_performance_benchmarks() -> Result<()> {
    println!("⚡ Testing performance benchmarks...");

    let service = start_mcp_server().await?;

    // Test basic responsiveness via tool listing
    let start_time = std::time::Instant::now();

    let _tools = service.list_tools(Default::default()).await?;

    let list_tools_time = start_time.elapsed();
    println!("⏱️ list_tools time: {:?}", list_tools_time);

    // Listing tools should be fast (< 5 seconds)
    assert!(
        list_tools_time < Duration::from_secs(5),
        "Tool discovery should be fast"
    );

    service.cancel().await?;
    println!("🎉 Performance benchmark test passed!");
    Ok(())
}
