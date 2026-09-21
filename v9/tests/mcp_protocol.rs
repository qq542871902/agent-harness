use mini_harness_v9::{
    llm::ToolCall,
    mcp::{McpConfig, McpLimits, McpManager, McpServerConfig},
    tools::ToolRegistry,
};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

fn config(scenario: &str) -> McpConfig {
    McpConfig {
        version: 1,
        servers: vec![McpServerConfig {
            name: "mock".into(),
            enabled: true,
            command: env!("CARGO_BIN_EXE_v9-mcp-test-server").into(),
            args: vec![scenario.into()],
            working_directory: None,
            env_allowlist: vec![],
        }],
    }
}

fn limits() -> McpLimits {
    McpLimits {
        startup_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(1),
        shutdown_timeout: Duration::from_millis(100),
        max_line_bytes: 4096,
        max_result_bytes: 2048,
        max_stderr_bytes: 1024,
    }
}

async fn connected(scenario: &str) -> (McpManager, ToolRegistry) {
    let mut registry = ToolRegistry::new();
    let manager = McpManager::connect_with_limits(&config(scenario), &mut registry, limits())
        .await
        .unwrap();
    (manager, registry)
}

#[tokio::test]
async fn paginates_denies_server_requests_and_maps_success() {
    let (mut manager, registry) = connected("success").await;
    let names = registry.mcp_tool_names();
    assert_eq!(names, vec!["mcp__mock__echo", "mcp__mock__first"]);
    let output = registry
        .execute(&ToolCall {
            id: "1".into(),
            name: "mcp__mock__echo".into(),
            arguments: json!({"x":1}),
        })
        .await
        .unwrap();
    assert!(output.success);
    assert!(output.content.contains("hello") && output.content.contains("\"answer\":42"));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn maps_is_error_without_transport_failure() {
    let (mut manager, registry) = connected("is-error").await;
    let output = registry
        .execute(&ToolCall {
            id: "1".into(),
            name: "mcp__mock__echo".into(),
            arguments: json!({}),
        })
        .await
        .unwrap();
    assert!(!output.success);
    assert_eq!(output.content, "remote failure");
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn bounds_timeout_malformed_and_oversize_responses() {
    for (scenario, expected) in [
        ("timeout", "timed out"),
        ("malformed", "invalid JSON"),
        ("oversize", "line exceeds"),
    ] {
        let (mut manager, registry) = connected(scenario).await;
        let error = registry
            .execute(&ToolCall {
                id: "1".into(),
                name: "mcp__mock__echo".into(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{scenario}: {error:#}"
        );
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn timeout_poisoning_prevents_reuse_after_indeterminate_call() {
    let (mut manager, registry) = connected("timeout").await;
    let first = registry
        .execute(&ToolCall {
            id: "first".into(),
            name: "mcp__mock__echo".into(),
            arguments: json!({}),
        })
        .await
        .unwrap_err();
    assert!(format!("{first:#}").contains("timed out"));

    let second = registry
        .execute(&ToolCall {
            id: "second".into(),
            name: "mcp__mock__echo".into(),
            arguments: json!({}),
        })
        .await
        .unwrap_err();
    assert!(format!("{second:#}").contains("closed"));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejects_discovered_tool_name_collisions() {
    let mut registry = ToolRegistry::new();
    let error = McpManager::connect_with_limits(&config("duplicate"), &mut registry, limits())
        .await
        .err()
        .expect("duplicate discovered tools must fail startup");
    assert!(format!("{error:#}").contains("already registered"));
}

#[test]
fn test_server_path_is_absolute() {
    assert!(PathBuf::from(env!("CARGO_BIN_EXE_v9-mcp-test-server")).is_absolute());
}
