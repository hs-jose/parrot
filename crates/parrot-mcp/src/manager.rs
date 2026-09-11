//! MCP server 生命周期编排（spec §3.3）：
//! 每 server 一个 tokio task：spawn → 握手 → 枚举 → 注册（热注册，下一 turn 可见）
//! → monitor（进程死亡信号 + is_closed 轮询兜底 + list_changed 重枚举）。
//! 启动失败只隔离该 server：warn + McpNotice(Failed)，不阻断 daemon。
//!
//! 崩溃检测说明：rmcp `RunningService::is_closed()` 在子进程死亡后仍为 false
//! （服务句柄未 close、cancellation token 未取消，已实测确认），因此崩溃检测
//! 不依赖它，而是用 `CrashWatchTransport` 在 transport `receive()` 遇到
//! stdout EOF（= 进程退出）时发信号给 monitor；1s 的 is_closed 轮询仅作兜底。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_config::McpServerConfig;
use parrot_core::tool::ToolRegistry;
use parrot_protocol::types::{McpServerState, McpServerStatusWire};
use rmcp::service::{RoleClient, RxJsonRpcMessage, ServiceExt, TxJsonRpcMessage};
use rmcp::transport::{which_command, TokioChildProcess, Transport};
use tokio::sync::{broadcast, mpsc, OnceCell, RwLock};

use crate::adapter::{
    map_service_error, qualified_tool_name, ListChangeNotify, McpService, McpTool,
};

/// manager 与后台 task 共享的状态映射（daemon 侧快照查询用）。
pub type McpStatusMap = Arc<RwLock<HashMap<String, McpServerStatusWire>>>;

struct ServerRuntime {
    id: String,
    /// 握手完成前为空（此时 shutdown 只 abort 任务，transport Drop 兜底杀子进程）。
    service: Arc<OnceCell<McpService>>,
    tool_names: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

/// transport 包装：子进程死亡时（stdout EOF → `receive()` 返回 None）发出信号。
/// 信号经 channel 送达 monitor，实现崩溃热下线（不阻塞 serve 循环）。
struct CrashWatchTransport {
    inner: TokioChildProcess,
    death_tx: mpsc::Sender<()>,
}

impl Transport<RoleClient> for CrashWatchTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        let death_tx = self.death_tx.clone();
        async move {
            let msg = self.inner.receive().await;
            if msg.is_none() {
                // 只发一次信号；通道容量 4 足够，不必阻塞 serve 循环。
                let _ = death_tx.try_send(());
            }
            msg
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

pub struct McpManager {
    /// 状态变更通知（daemon 转发给客户端 UI）。晚订阅者收不到历史通知——
    /// 补救手段是 `ListMcpServers` 查询。
    notices: broadcast::Sender<McpServerStatusWire>,
    status: McpStatusMap,
    registry: Arc<ToolRegistry>,
    servers: Vec<ServerRuntime>,
}

/// 非阻塞启动：每 server 派一个后台 task（spawn→握手→枚举→注册→monitor），
/// 立即返回 manager。
pub async fn start_all(registry: Arc<ToolRegistry>, servers: Vec<McpServerConfig>) -> McpManager {
    let (notices, _) = broadcast::channel(16);
    let status: McpStatusMap = Arc::new(RwLock::new(HashMap::new()));
    let mut runtimes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for server in servers {
        if server.id.is_empty() {
            tracing::warn!("MCP server id 为空，跳过");
            set_status_map(
                &status,
                &notices,
                "",
                McpServerState::Failed,
                "配置错误: id 为空",
                0,
            )
            .await;
            continue;
        }
        if !seen.insert(server.id.clone()) {
            tracing::warn!("MCP server id '{}' 重复，跳过", server.id);
            set_status_map(
                &status,
                &notices,
                &server.id,
                McpServerState::Failed,
                "配置错误: id 重复",
                0,
            )
            .await;
            continue;
        }
        let service_cell = Arc::new(OnceCell::new());
        let tool_names = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(run_server_task(
            server.clone(),
            Arc::clone(&registry),
            notices.clone(),
            Arc::clone(&status),
            Arc::clone(&service_cell),
            Arc::clone(&tool_names),
        ));
        runtimes.push(ServerRuntime {
            id: server.id.clone(),
            service: service_cell,
            tool_names,
            task,
        });
    }
    McpManager {
        notices,
        status,
        registry,
        servers: runtimes,
    }
}

impl McpManager {
    /// 订阅状态变更通知（connection handler 建立时调用）。
    pub fn subscribe(&self) -> broadcast::Receiver<McpServerStatusWire> {
        self.notices.subscribe()
    }

    /// 当前全部 server 状态快照（`ListMcpServers` 应答用，按 id 排序）。
    pub async fn status_snapshot(&self) -> Vec<McpServerStatusWire> {
        let map = self.status.read().await;
        let mut list: Vec<McpServerStatusWire> = map.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// 优雅关闭：abort monitor → close 每个连接（最多等 5s，transport 层
    /// 超时后 kill 子进程）→ 批量 unregister 工具 → 广播 Stopped。
    pub async fn shutdown(&self) {
        for rt in &self.servers {
            rt.task.abort();
            if let Some(service) = rt.service.get() {
                let mut svc = service.write().await;
                let _ = svc.close_with_timeout(Duration::from_secs(5)).await;
            }
            let names: Vec<String> = rt.tool_names.lock().unwrap().drain(..).collect();
            for name in names {
                self.registry.unregister(&name).await;
            }
            self.set_status(&rt.id, McpServerState::Stopped, "daemon 关闭", 0)
                .await;
        }
    }

    async fn set_status(&self, id: &str, state: McpServerState, detail: &str, tool_count: u32) {
        set_status_map(&self.status, &self.notices, id, state, detail, tool_count).await;
    }
}

async fn run_server_task(
    server: McpServerConfig,
    registry: Arc<ToolRegistry>,
    notices: broadcast::Sender<McpServerStatusWire>,
    status: McpStatusMap,
    service_cell: Arc<OnceCell<McpService>>,
    tool_names: Arc<Mutex<Vec<String>>>,
) {
    set_status_map(
        &status,
        &notices,
        &server.id,
        McpServerState::Starting,
        "",
        0,
    )
    .await;

    let (list_tx, mut list_rx) = mpsc::channel::<()>(4);
    let (death_tx, mut death_rx) = mpsc::channel::<()>(4);
    let notify_handler = ListChangeNotify { tx: list_tx };

    let startup = Duration::from_secs(server.startup_timeout_seconds);
    let attempt = tokio::time::timeout(startup, async {
        let transport =
            make_transport(&server, death_tx).map_err(|e| format!("spawn 失败: {e}"))?;
        let service = notify_handler
            .clone()
            .serve(transport)
            .await
            .map_err(|e| format!("握手失败: {e}"))?;
        let tools = service
            .list_all_tools()
            .await
            .map_err(|e| map_service_error(&server.id, &e))?;
        Ok::<_, String>((service, tools))
    })
    .await;

    let (service, tools) = match attempt {
        Err(_) => {
            let detail = format!("启动超时(超过 {}s)", server.startup_timeout_seconds);
            tracing::warn!(server = %server.id, "{detail}");
            set_status_map(
                &status,
                &notices,
                &server.id,
                McpServerState::Failed,
                &detail,
                0,
            )
            .await;
            return; // transport Drop → ChildWithCleanup kill 子进程
        }
        Ok(Err(e)) => {
            tracing::warn!(server = %server.id, "{e}");
            set_status_map(&status, &notices, &server.id, McpServerState::Failed, &e, 0).await;
            return;
        }
        Ok(Ok(v)) => v,
    };

    let service: McpService = Arc::new(RwLock::new(service));
    let _ = service_cell.set(Arc::clone(&service));

    let names = register_tools(
        &server.id,
        &registry,
        &tools,
        &service,
        server.call_timeout_seconds,
    )
    .await;
    let count = names.len() as u32;
    *tool_names.lock().unwrap() = names;
    set_status_map(
        &status,
        &notices,
        &server.id,
        McpServerState::Connected,
        "",
        count,
    )
    .await;

    // monitor：进程死亡信号（事件驱动）+ is_closed 轮询(1s，兜底) + list_changed 重枚举
    loop {
        tokio::select! {
            _ = death_rx.recv() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if service.read().await.is_closed() {
                    break;
                }
            }
            _ = list_rx.recv() => {
                let tools = match service.read().await.list_all_tools().await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(server = %server.id, "list_changed 重枚举失败: {}", map_service_error(&server.id, &e));
                        continue;
                    }
                };
                let old: Vec<String> = tool_names.lock().unwrap().drain(..).collect();
                for name in old {
                    registry.unregister(&name).await;
                }
                let names = register_tools(&server.id, &registry, &tools, &service, server.call_timeout_seconds).await;
                let count = names.len() as u32;
                *tool_names.lock().unwrap() = names;
                set_status_map(&status, &notices, &server.id, McpServerState::Connected, "", count).await;
            }
        }
    }

    // server 进程退出：热下线整组工具并广播 Stopped
    let names: Vec<String> = tool_names.lock().unwrap().drain(..).collect();
    for name in names {
        registry.unregister(&name).await;
    }
    set_status_map(
        &status,
        &notices,
        &server.id,
        McpServerState::Stopped,
        "server 进程已退出",
        0,
    )
    .await;
}

fn make_transport(
    server: &McpServerConfig,
    death_tx: mpsc::Sender<()>,
) -> std::io::Result<CrashWatchTransport> {
    let mut cmd = which_command(&server.command)?;
    cmd.args(&server.args);
    for (k, v) in &server.env {
        cmd.env(k, v);
    }
    let inner = TokioChildProcess::new(cmd)?;
    Ok(CrashWatchTransport { inner, death_tx })
}

/// 注册整组工具（按 qualified 名去重：已被占用的名字跳过，只隔离单个工具）。
async fn register_tools(
    server_id: &str,
    registry: &ToolRegistry,
    tools: &[rmcp::model::Tool],
    service: &McpService,
    call_timeout_secs: u64,
) -> Vec<String> {
    let mut names = Vec::new();
    for def in tools {
        let qualified = qualified_tool_name(server_id, &def.name);
        if registry.get(&qualified).await.is_some() {
            tracing::warn!(server = server_id, tool = %qualified, "MCP 工具名冲突，跳过注册");
            continue;
        }
        registry
            .register(Arc::new(McpTool::new(
                server_id,
                def,
                Arc::clone(service),
                call_timeout_secs,
            )))
            .await;
        names.push(qualified);
    }
    names
}

async fn set_status_map(
    status: &McpStatusMap,
    notices: &broadcast::Sender<McpServerStatusWire>,
    id: &str,
    state: McpServerState,
    detail: &str,
    tool_count: u32,
) {
    let entry = McpServerStatusWire {
        id: id.to_string(),
        state,
        detail: detail.to_string(),
        tool_count,
    };
    status.write().await.insert(id.to_string(), entry.clone());
    let _ = notices.send(entry);
}
