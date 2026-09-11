//! MCP client 完整生命周期：spawn→握手→注册→调用→崩溃热下线→优雅关闭。

use parrot_config::McpServerConfig;
use parrot_core::tool::ToolRegistry;
use parrot_mcp::{McpManager, McpServerState};
use std::sync::Arc;
use std::time::Duration;

fn mock_config() -> McpServerConfig {
    McpServerConfig {
        id: "mock".into(),
        command: env!("CARGO_BIN_EXE_parrot_mcp_mock_server").into(),
        args: vec![],
        env: Default::default(),
        startup_timeout_seconds: 30,
        call_timeout_seconds: 30,
        require_confirmation: false,
    }
}

async fn wait_state(
    manager: &McpManager,
    id: &str,
    want: McpServerState,
    secs: u64,
) -> parrot_protocol::types::McpServerStatusWire {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let snap = manager.status_snapshot().await;
        if let Some(s) = snap.iter().find(|s| s.id == id) {
            if s.state == want {
                return s.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout waiting for {id} -> {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn spawn_handshake_register_call() {
    let registry = Arc::new(ToolRegistry::new());
    let manager = parrot_mcp::start_all(Arc::clone(&registry), vec![mock_config()]).await;

    let status = wait_state(&manager, "mock", McpServerState::Connected, 30).await;
    assert_eq!(status.tool_count, 4, "echo/fail/exit/extend");

    let echo = registry
        .get("mcp__mock__echo")
        .await
        .expect("echo registered");
    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let out = echo
        .call(serde_json::json!({"message": "hi"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.content, "echo: hi");
    assert!(!out.is_error);

    let fail = registry
        .get("mcp__mock__fail")
        .await
        .expect("fail registered");
    let out = fail.call(serde_json::json!({}), &ctx).await.unwrap();
    assert!(out.is_error);
    assert_eq!(out.content, "mock failure detail", "server 错误全文保真");

    manager.shutdown().await;
    let status = wait_state(&manager, "mock", McpServerState::Stopped, 10).await;
    assert_eq!(status.tool_count, 0);
    assert!(
        registry.get("mcp__mock__echo").await.is_none(),
        "关闭后工具下线"
    );
}

#[tokio::test]
async fn crash_triggers_hot_unregister_with_notice() {
    let registry = Arc::new(ToolRegistry::new());
    let manager = parrot_mcp::start_all(Arc::clone(&registry), vec![mock_config()]).await;
    let mut rx = manager.subscribe();
    wait_state(&manager, "mock", McpServerState::Connected, 30).await;

    let exit = registry
        .get("mcp__mock__exit")
        .await
        .expect("exit registered");
    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let _ = exit.call(serde_json::json!({}), &ctx).await.unwrap();

    let mut stopped = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while stopped.is_none() && tokio::time::Instant::now() < deadline {
        if let Ok(notice) = rx.try_recv() {
            if notice.id == "mock" && notice.state == McpServerState::Stopped {
                stopped = Some(notice);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(stopped.is_some(), "崩溃后应收到 McpNotice(Stopped)");
    assert!(
        registry.get("mcp__mock__echo").await.is_none(),
        "崩溃后工具热下线"
    );
}

#[tokio::test]
async fn bad_command_is_isolated_with_failed_notice() {
    let registry = Arc::new(ToolRegistry::new());
    let mut bad = mock_config();
    bad.id = "nope".into();
    bad.command = "this_binary_definitely_does_not_exist_12345".into();
    let manager = parrot_mcp::start_all(Arc::clone(&registry), vec![bad, mock_config()]).await;
    let mut rx = manager.subscribe();

    let failed = wait_state(&manager, "nope", McpServerState::Failed, 30).await;
    assert!(
        !failed.detail.is_empty(),
        "失败必须带具体原因: {:?}",
        failed.detail
    );

    let ok = wait_state(&manager, "mock", McpServerState::Connected, 30).await;
    assert_eq!(ok.tool_count, 4, "坏 server 不影响好 server");

    let mut got_failed_notice = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(n) = rx.try_recv() {
            if n.id == "nope" && n.state == McpServerState::Failed {
                got_failed_notice = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(got_failed_notice, "失败应广播 McpNotice(Failed)");
    manager.shutdown().await;
}

/// list_changed 全链路：extend 调用 → server 广播 notifications/tools/list_changed
/// → client ClientHandler::on_tool_list_changed → manager 重枚举（unregister 全组
/// → 重新注册 → Connected 状态重新广播）。swap 后旧工具保留、extend 自身下线、
/// extra 上线。
#[tokio::test]
async fn list_changed_adds_and_swaps_tools() {
    let registry = Arc::new(ToolRegistry::new());
    let manager = parrot_mcp::start_all(Arc::clone(&registry), vec![mock_config()]).await;
    let mut rx = manager.subscribe();
    wait_state(&manager, "mock", McpServerState::Connected, 30).await;
    // 排空初始通知：此后收到的 Connected(tool_count=4) 必来自重枚举
    while rx.try_recv().is_ok() {}

    let extend = registry
        .get("mcp__mock__extend")
        .await
        .expect("extend registered");
    let ctx = parrot_core::tool::ToolContext::new(std::path::PathBuf::from("."), 1024);
    let out = extend.call(serde_json::json!({}), &ctx).await.unwrap();
    assert_eq!(out.content, "extended");
    assert!(!out.is_error);

    // 重枚举把 extend 换成 extra；echo 等未变更工具必须保留
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let extra = registry.get("mcp__mock__extra").await.is_some();
        let echo = registry.get("mcp__mock__echo").await.is_some();
        if extra && echo {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout waiting for re-enumeration"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        registry.get("mcp__mock__extend").await.is_none(),
        "swap 后 extend 应下线"
    );

    let mut re_broadcast = false;
    while let Ok(n) = rx.try_recv() {
        if n.id == "mock" && n.state == McpServerState::Connected && n.tool_count == 4 {
            re_broadcast = true;
        }
    }
    assert!(re_broadcast, "重枚举后应重新广播 Connected(tool_count=4)");

    manager.shutdown().await;
}

/// start_all 丢弃的 server（空 id / 重复 id）必须在 /mcp 状态快照里可见并带原因。
#[tokio::test]
async fn dropped_servers_visible_in_status() {
    let registry = Arc::new(ToolRegistry::new());
    let mut bad = mock_config();
    bad.id = "nope".into();
    bad.command = "this_binary_definitely_does_not_exist_12345".into();
    let empty = McpServerConfig {
        id: "".into(),
        ..mock_config()
    };
    let manager = parrot_mcp::start_all(registry, vec![empty, bad.clone(), bad]).await;

    let snap = manager.status_snapshot().await;
    let empty_entry = snap
        .iter()
        .find(|s| s.id.is_empty())
        .expect("空 id server 应可见");
    assert_eq!(empty_entry.state, McpServerState::Failed);
    assert!(
        empty_entry.detail.contains("id 为空"),
        "detail: {:?}",
        empty_entry.detail
    );

    let nope = snap
        .iter()
        .find(|s| s.id == "nope")
        .expect("重复 id server 应可见");
    assert_eq!(nope.state, McpServerState::Failed);
    assert!(!nope.detail.is_empty(), "detail: {:?}", nope.detail);

    manager.shutdown().await;
}
