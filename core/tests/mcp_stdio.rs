//! Integration tests for the MCP client against a real stdio subprocess.
//!
//! These cover the path that until now had no test at all: spawn, initialize,
//! discover, namespace, call, and propagate `isError`. A fake server is used
//! so the test is deterministic and offline.

use jarvis_core::config::{Config, McpConfig, McpServerConfig};
use jarvis_core::mcp::client::McpClient;
use jarvis_core::planner;
use jarvis_core::router::Route;
use jarvis_core::tools::ToolRegistry;
use serde_json::json;
use std::path::PathBuf;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo_mcp.py")
}

fn config(always_offer: bool) -> Config {
    Config {
        mcp: Some(McpConfig {
            servers: vec![McpServerConfig {
                name: "echo".to_string(),
                command: "python3".to_string(),
                args: vec![fixture().to_string_lossy().to_string()],
                env: Default::default(),
                enabled: true,
                always_offer,
            }],
        }),
        ..Config::default()
    }
}

#[tokio::test]
async fn discovers_and_namespaces_tools() {
    let client = McpClient::new();
    client
        .start_from_config(&config(false))
        .await
        .expect("server starts");

    let tools = client.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.full_name.as_str()).collect();
    assert!(
        names.contains(&"mcp.echo.echo"),
        "expected namespaced tool, got {:?}",
        names
    );
    assert!(names.contains(&"mcp.echo.boom"));
    assert!(client.has_server("echo"));

    let echo = tools
        .iter()
        .find(|t| t.full_name == "mcp.echo.echo")
        .expect("echo tool");
    assert!(!echo.always_offer);
}

#[tokio::test]
async fn always_offer_is_propagated_to_discovered_tools() {
    let client = McpClient::new();
    client
        .start_from_config(&config(true))
        .await
        .expect("server starts");

    assert!(client.list_tools().iter().all(|t| t.always_offer));
}

#[tokio::test]
async fn calls_a_tool_and_flattens_the_content() {
    let client = McpClient::new();
    client
        .start_from_config(&config(false))
        .await
        .expect("server starts");

    let output = client
        .call_tool("echo", "echo", json!({"text": "hello"}))
        .await
        .expect("call succeeds");
    assert_eq!(output, "echo: hello");
}

#[tokio::test]
async fn a_tool_error_becomes_an_err() {
    let client = McpClient::new();
    client
        .start_from_config(&config(false))
        .await
        .expect("server starts");

    let error = client
        .call_tool("echo", "boom", json!({}))
        .await
        .expect_err("isError must not look like success");
    assert!(error.to_string().contains("exploded on purpose"));
}

#[tokio::test]
async fn calling_an_unknown_server_is_an_error_not_a_panic() {
    let client = McpClient::new();
    let error = client
        .call_tool("nope", "tool", json!({}))
        .await
        .expect_err("unknown server");
    assert!(error.to_string().contains("not found"));
}

#[tokio::test]
async fn a_broken_server_does_not_stop_the_others() {
    let mut config = config(false);
    config
        .mcp
        .as_mut()
        .expect("mcp")
        .servers
        .push(McpServerConfig {
            name: "broken".to_string(),
            command: "definitely-not-a-real-command".to_string(),
            args: vec![],
            env: Default::default(),
            enabled: true,
            always_offer: false,
        });

    let client = McpClient::new();
    client
        .start_from_config(&config)
        .await
        .expect("startup never fails wholesale");

    let names: Vec<String> = client
        .list_tools()
        .into_iter()
        .map(|t| t.full_name)
        .collect();
    assert!(names.contains(&"mcp.echo.echo".to_string()));
}

#[tokio::test]
async fn planner_offers_web_tools_for_a_web_question() {
    let client = McpClient::new();
    client
        .start_from_config(&config(false))
        .await
        .expect("server starts");
    let mcp = client.list_tools();

    // A non-web server must not be offered for a web question.
    let plan = planner::plan(
        "what is the latest news about rust",
        Route::Model,
        &ToolRegistry::new(),
        &mcp,
    );
    assert!(!plan.allows("mcp.echo.echo"));

    // Naming the server makes it available, and the prompt lists exactly the
    // tools the provider request carries.
    let plan = planner::plan(
        "use the echo server to say hi",
        Route::Model,
        &ToolRegistry::new(),
        &mcp,
    );
    assert!(plan.allows("mcp.echo.echo"));
    let prompt = planner::render_tool_prompt(&plan.tools).expect("prompt");
    assert!(prompt.contains("mcp.echo.echo"));
    assert_eq!(plan.tools.len(), plan.schemas().len());
}
