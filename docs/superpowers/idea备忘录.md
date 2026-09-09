# idea备忘录

记录一些迭代过程中的想法。后面会记录每条想法的状态（完成、不进行、进行中等）和重要程度（p0，p1，p2，p3）

## 核心部分
- [ ] p1 daemon 进程的优雅退出与崩溃兜底。当前 `src/daemon/main.rs` 没有接 `tokio::signal::ctrl_c()`，Ctrl+C 直接被 runtime 砍掉，`AgentEndGuard::drop` 只 `try_send` 不落盘（注释明确写了"Drop-path can't safely do disk IO"）。后果：daemon 被杀时 in-flight turn 的 `AgentEnd` 永远进不了 `events.log`，后续 resume 看到"日志停在 TurnEnd/MessageEnd 但没 AgentEnd"难以区分"正常关闭"和"崩溃"。需要：1) 接 `tokio::signal::ctrl_c()` 触发 `SessionManager::shutdown_all()`（abort 各 session 任务并短暂 await 让 `fire_and_drop` 跑完落盘 `AgentEnd`）；2) 评估 `EventsAfterAgentEnd` 检查对"合法 resume 在 AgentEnd 之后追加 AgentStart"的误判（当前因为 AgentEnd 几乎从不落盘所以没暴露，加上优雅退出后会暴露）。注意：TUI 端 Ctrl+C 关的是 thin WS client，daemon 不受影响，不在此项范围内。
- [ ] p2 `EventLog` 移出 `parrot-core`，兑现 zero-IO 原则。当前 `crates/parrot-core/src/event_log.rs` 全部用 `std::fs` 同步 IO（append/replay/write_snapshot/rewrite_truncated），违反 AGENTS.md "parrot-core has zero IO: no reqwest, no tokio::fs, no std::env" 的约束 —— 只是因为 `std::fs` 是同步的、不需要 runtime，才"凑合"留在 core 里。代价：1) engine 任务的 worker 线程会被每次 event 落盘短暂阻塞（multi-thread runtime 下不至于卡死整个 executor，但大文件/网络盘/encryption-at-rest 场景会放大）；2) 边界契约不纯，测试时无法注入 mock sink。建议方案：core 暴露 `trait EventSink { fn append(...) -> ...; }`，daemon 层用 `tokio::fs` 实现 AsyncEventLog（磁盘操作丢 blocking pool，不阻塞 executor），engine 只持有 trait 对象。一次性重构可以同时解决这个 + 生命周期 hook 扩展点（见 engine 规范化那条）。与 `AgentEndGuard::Drop` 的限制需要一起评估 —— Drop 里不能 `.await`，所以 Guard 路径要么走 `spawn_blocking` + `try_send` 通知，要么保留同步 fs 单独放行。
- [ ] p0 上下文管理
- [ ] p3 系统提示词怎么搞比较好？做一个单独的文档注入  
- [ ] p2 会话记录可以保留一个父会话id，用来记录对话时的分支信息，比如从上一个会话派生两个新会话分支，后续也可以基于这个id回到上一个父节点
- [ ] p2 server端和client端的交互现在是简单的ws，可以封装成通用逻辑？后面接入其他端 TUI、IM 应该怎么做？
- [ ] p2 rust的单元测试似乎有些分散命名也不统一，是不是需要重新在整合一下？
- [ ] p2 我的工具现在是mcp协议吗？现在都是rust开发的内置tool，后面如何接入ts生态？shell_exec是不是需要兼容多个shell？
- [ ] p2 skill是一个很火的概念，如何支持
- [x] p1 tool call没有返回给client工具调用的参数和tool call结果，需要增加，这部分是不是也需要增加server和clinet的交互协议
- [ ] p1 安全问题，agent的沙箱环境需要补充，目前只限制了一些常见危险命令
- [ ] p1 可扩展性 我参考了pi的生命周期设计，但是没有提供各个生命周期的可扩展点，需要补充关键节点的hook能力。这部分代码可能需要在engine中实现，engine的ReAct流程是不是需要更加的规范化结构化（能不能用类似编排框架的思路？）
- [ ] p1 大模型给出了[[\docs\superpowers\业界对比调研.md]] 学习一下里面的优秀项目思路和架构
- [ ] p1 llm provider 配置化优化，现在的配置文件是一个的toml文件，只支持一个provider，需要修改为支持多个。并且其中还需要增加字段判断提供商接口类型A\接口还是OpenAi接口还是别的。id字段需要唯一，因为需要一个供应商配多个apikey?
- [ ] p1 定义一个目录用于存放配置文件
- [ ] p0 关键 整体思路总结。思路是按照 数据模型设计->项目核心流程->核心流程异常处理（保证框架可用）关键正确性、核心约束->项目稳定性 可用性 可观测性，项目整体流程跑通（类似做好整体的兜底）
- [ ] p0 上下文压缩窗口减预留触发、char/4 估算、生成结构化摘要。真正拉开差距的是三个根本取舍：缓存、结构、边界。
这三样直接决定了各自的能力天花板。https://blog.silencestar.com/posts/context-compaction-3way/
- [x] p0 考虑多端兼容部署。rust交叉编译
- [x] p0 当前的项目需要启动parrotd才能运行，如果之后做打包分发如何控制守护进程的启动和停止？自动管理 daemon 生命周期？   已经修改为通过parrot启动子进程方式，parrot结束则守护进程一起结束

## provider

- [ ] 代码里面好多魔数，max_tokens: config.max_tokens.unwrap_or(8192),tokio::sync::mpsc::channel(64)  ModelInfo {
                id: "claude-sonnet-4-6".to_string(),
                name: "Claude Sonnet 4.6".to_string(),
                provider: "anthropic".to_string(),
                context_window: 200000,
                max_output_tokens: 8192,
            }, 搞成可以配置的
- [ ] 模型思维链展示

## tool

- [ ] tool优化，比如读取工具可以增加参数可以读取指定开始结束行代码？工具的权限验证基于hook做  
- [ ] tool结果超长截断，是否需要在某个地方保留完整的toolcall结果，供后续分析使用？对工具进行抽象，增加工具调用前后钩子函数和策略
- [x] 需要一个能实现精准代码修改的tool file_edit，进行字符串精准匹配替换（或通过 AST 语法树修改 AST太麻烦？）

## 选型
- [ ] rust版本升级

## TUI
- [ ] p2 多行粘贴体验与机制确认。Windows 下 crossterm 事件源不产出 `Event::Paste`（走 Console API，`sys/windows/parse.rs` 无任何 bracketed paste 解析），`EnableBracketedPaste` 形同虚设，粘贴合成为逐字符按键事件。2026-09-09 实测：单行/多行粘贴在输入框中均正常显示、不会提前发送——推测 ConPTY 把粘贴的换行合成为带修饰键的 Enter（`\n` 惯例映射 Ctrl+J 类），恰好命中 TUI 的 Shift+Enter/Ctrl+Enter/Ctrl+J 换行分支而非裸 Enter 提交分支（属巧合性正确，机制未经按键日志确证）。可选改进：加临时按键日志确证机制；根治依赖 crossterm 支持 Windows bracketed paste。当前按用户决定保持现状。
