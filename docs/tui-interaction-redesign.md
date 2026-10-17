# TUI 交互重构：Codex 架构 + 彩色多代理房间

状态：设计稿 v2（身份与交互决策已定，见 §8）
范围：`src/bin/mio_cli/` 交互层，以及新增的去中心化多代理协作原语
参考：codex-rs/tui（主参考，同为 Rust）、pi、reasonix、agy

## 1. 现状

| 维度 | 现状 | 证据 |
| --- | --- | --- |
| 输入行 | 单行缓冲，`\r\x1b[2K` 整行重绘 | `terminal.rs` `InteractiveEditor::render` |
| 多行 / 粘贴 | 无多行，无 bracketed paste | 无 `?2004h` |
| 历史 | 内存 `Vec<String>`，退出即丢 | `InteractiveEditor::history` |
| 宽度 | 手写 `char_display_width`，CJK 区间不全 | `terminal.rs:121` |
| 输出 | `print!` 直写 stdout，与输入行抢光标 | `runner.rs` `CliHost::on_event` |
| 状态 / 弹层 | 无状态栏；审批是 `read_line` 的 `[y/N]` | `CliHost::interactive_approval` |
| 多代理 | 只有父 -> 子委派（`SubagentManager`），用户只能和主代理说话 | `src/subagent.rs` |

## 2. 参考项目取舍

| 项目 | 取什么 | 不取什么 |
| --- | --- | --- |
| **codex-rs/tui**（Rust + ratatui，Apache-2.0） | 整体架构：inline viewport、历史写入终端 scrollback、composer 背景色块、footer 提示、`Alt+←/→` 切换代理、markdown 流式渲染、审批弹层 | app-server 协议层、realtime、pets、onboarding 等外围功能 |
| **pi** | 差分渲染只重绘变化行；工具调用按状态着色（pending / ok / error）；footer 显示 cwd、分支、token、上下文占比；`@` 文件补全 | TS 扩展运行时 |
| **reasonix** | 写文件前自动快照、`/rewind`（Mio 已有 checkpoints，TUI 只需把它显性化）；`/pro` 这类单回合模型切换 | DeepSeek 专用缓存策略 |
| **agy** | 命令面板式交互、计划 / 执行模式切换的呈现方式 | 编辑器集成 |
| **jj** | Agent 任务绑定稳定 change ID、每个编码 Agent 使用独立 workspace、operation log 恢复、冲突可持久化，并通过 Git backend 兼容现有远程 | 不暴露底层 Git 命令作为 Mio 工作流 |

### 2.1 Codex 关键实现（已读源码，`.cache/codex-src/codex-rs/tui`）

- **不进 alt-screen**。`custom_terminal.rs` 是 ratatui `Terminal` 的派生版，只管理屏幕底部一块 viewport（composer + 状态 + footer）。
- **定稿内容写入 scrollback**。`insert_history.rs` 用 `SetScrollRegion(1..viewport.top)` 把滚动区限制在 viewport 上方，在其中 `\r\n` 打印定稿行，然后 `ResetScrollRegion`。viewport 不在屏幕底部时，先用 `ESC M` 下推。效果是历史可以原生滚动和鼠标复制，输入框始终固定在底部。
- **流式两段式**。`streaming/controller.rs` + `markdown_stream.rs`：正在生成的段落留在 viewport 里重绘，遇到换行 / 块结束才提交到 scrollback，避免重排已提交内容。
- **composer 是色块，没有边框**。`style.rs::user_message_bg` 把前景色按 alpha 混到终端背景上（暗色背景用白色，alpha 0.12），`chat_composer.rs` 用 `Block::default().style(..)` 铺底色。
- **多代理导航**。`multi_agents.rs`：代理有 nickname / role，`Alt+←/→` 快速切换，macOS 下空 composer 时回退到 `Alt+b/f`。
- 依赖：`ratatui`（`scrolling-regions` 等 feature）、`crossterm`（`bracketed-paste`、`event-stream`）、`pulldown-cmark`、`textwrap`、`unicode-width`、`unicode-segmentation`、`diffy`。

许可：codex 是 Apache-2.0，`custom_terminal.rs` 另含 ratatui 的 MIT 声明。Mio 按其思路**重写**，不直接复制文件；如确需移植片段，按项目约定在 `licenses/` 和 `CREDITS.md` 登记并保留 NOTICE。

## 3. 渲染架构（照 Codex）

```
┌ 终端 scrollback（原生滚动 / 复制）──────────────┐
│ ▍你 → @A @B：协商后端与 API 的协作                 │
│ ● @A · API  提议：/api/v1/tasks 由我来定 schema … │  定稿 cell
│ ● @B · 鉴权  同意 rev 1，鉴权中间件归我 …          │  通过 insert_history 写入
│ ✓ @A · API  edit src/api/tasks.rs  (+42 −3)       │
├ inline viewport（每帧差分重绘）──────────────────┤
│ ● @B · 鉴权  正在生成…  12s · Esc 中断             │  live 流式 cell
│ ▌@A @B 接口改成分页                                │  composer：底色 = 目标代理色
│ ▌                                                 │
│  @A @B · 房间 backend-api · gpt-5 · 38% ctx       │  footer
└──────────────────────────────────────────────────┘
```

模块划分（全部在 `src/bin/mio_cli/tui/`，引擎不感知 TUI）：

| 文件 | 职责 |
| --- | --- |
| `terminal.rs` | 精简版 inline terminal：viewport 区域、差分 flush、resize |
| `history.rs` | scroll region 写入 scrollback（对应 codex `insert_history.rs`） |
| `app.rs` | 状态机：`AppEvent`（按键、粘贴、引擎事件、房间消息、tick）-> 状态 -> 渲染 |
| `composer.rs` | 多行编辑器、`@` 提及与文件补全、`/` 命令弹层、历史与 `Ctrl+R` |
| `cells.rs` | 历史 cell：用户、代理文本、思考（可折叠）、工具（按状态着色）、diff、系统提示 |
| `markdown.rs` | pulldown-cmark -> `Line`，流式增量提交 |
| `theme.rs` | Catppuccin Mocha 调色板与混色（§4） |
| `overlay.rs` | 审批三态、模型 / 思考深度选择、`/agents` 选择器 |

`runner.rs` 的 `CliHost::on_event` / `on_reasoning` 在 TUI 模式下改为向 `app` 发 `AppEvent`，不再直接 `print!`。`-p`、管道、`--mode json|rpc` 和非 TTY 维持现有输出，不受影响。

## 4. 主题：Catppuccin Mocha

### 4.1 基础色

| 用途 | 名称 | Hex |
| --- | --- | --- |
| 正文 | Text | `#cdd6f4` |
| 次要文字 / footer | Subtext0 | `#a6adc8` |
| 弱化 / 提示 | Overlay0 | `#6c7086` |
| 分隔线 | Surface1 | `#45475a` |
| 弹层底 | Surface0 | `#313244` |
| 参考背景 | Base | `#1e1e2e` |
| 深底 | Mantle / Crust | `#181825` / `#11111b` |

### 4.2 Agent 身份与角色

房间最多有 32 个 Agent，按加入房间的顺序获得稳定身份 `@A`…`@Z`、`@AA`…`@AF`。每个身份槽固定绑定一个主色、一个渐变终点和一个 xterm-256 回退色；同一房间生命周期内不复用仍有消息记录的身份。完整色值以 `src/bin/mio_cli/tui/palette.rs` 的固定表为唯一来源。

字母与颜色共同表示 **Agent 身份**。角色是独立的文字元数据，显示为 `@A · API`、`@B · 鉴权`；地址和消息路由始终使用稳定字母，不使用可能重名或变化的角色名。同一角色可以有多个 Agent，一个 Agent 的角色也可以调整，而历史消息的作者身份不会改变。

`@A`…`@H` 使用间距最大的八个 Mocha 强调色；`@I`…`@AF` 使用离线生成并固定的 OKLab 候选色。前面的身份优先获得更容易区分的颜色，因此小房间仍具有最高辨识度。用户使用 Lavender `#b4befe`，不占 Agent 身份槽。

颜色只是辅助线索。所有消息、状态和输入目标始终显示 `@字母`，`NO_COLOR` 下完全依靠字母与角色文字区分。成功、错误、警告等语义仍由图标和状态文字表达，不能只靠颜色。

### 4.3 composer 着色规则

- 底色 = `blend(目标代理色, 终端背景, 0.12)`；终端背景通过 OSC 11 查询，查不到时用 Mocha Base。
- 左侧 `▌` 竖条用目标代理原色；多个目标时竖条按行分段显示各自颜色，底色用第一个目标。
- 没有 `@` 时目标是“当前焦点代理”（`Alt+←/→` 切换），底色随焦点变化。
- `@` 提及高亮成对应颜色；`@` 后 `Tab` 补全 `@A`…`@AF`，候选项同时显示角色。
- 支持 `NO_COLOR` 与 256 色降级（codex `color.rs` 的 `perceptual_distance` 找最近色）。

### 4.4 动态状态

每个运行中的 Agent 有独立状态行，固定显示身份、角色、状态、耗时和可用操作，例如 `@B · 鉴权  正在生成… 12s · Esc 中断`。TrueColor 模式下，spinner 使用身份色，`正在生成`文字在该 Agent 的双色渐变上做 shimmer；256 色模式使用固定回退色和普通 spinner；`NO_COLOR` 只保留 spinner、文字与耗时。动画不得改变行宽或 Agent 排序。

## 5. 去中心化多代理：房间（Room）

### 5.1 和现有子代理的区别

| | 现有 `SubagentManager` | 房间 |
| --- | --- | --- |
| 拓扑 | 主代理 -> 子代理，树形 | 平级 peer，用户也是一个 peer |
| 谁和用户说话 | 只有主代理 | 任何被 `@字母` 寻址的 Agent |
| 结果去向 | 结构化结果回到父运行 | 消息投递到房间，被 `@` 的成员收到 |
| 分工 | 父代理决定 | 成员自己提议、认领、协商 |

房间不是“主代理换了个名字”：没有固定的调度者，谁收到消息谁行动。

### 5.2 消息模型

```
RoomMessage {
  id, room_id, seq,          // seq 单调，房间内全序
  from: AgentId | User,
  to: [AgentId] | All,       // @A @B -> 对应稳定 AgentId；无 @ -> 当前焦点
  kind: chat | propose | object | agree | claim | release | done,
  topic: Option<String>,     // 一次协商一个 topic
  revision: Option<u32>,     // propose 的版本号；agree / object 必须引用具体版本
  body: String,
}
```

代理通过一个新工具 `room_post` 发言（等价于 codex 的 collab 工具，但不经过父代理）。每个代理自己的上下文只包含：发给自己的消息、`All` 广播、所在 topic 的最新提案原文；不灌整个房间记录，控制成本。

### 5.3 协商流程（例：“@A 和 @B 协商后端与 API 的协作”）

1. 用户发 `@A @B 协商后端与 API 的协作`，消息路由到两个身份对应的 AgentId，topic 自动创建。
2. 两个代理各自被唤醒（事件驱动：只有信箱非空才跑一个回合）。
3. 任一方 `propose` 一个分工方案（rev 1），另一方 `object`（附理由）或 `agree rev 1`。
4. 修改后版本号递增，`agree` 只对引用的版本有效，避免“同意了旧方案”。
5. 所有参与者 `agree` 同一 revision 即收敛；随后各自 `claim` 自己的写入范围并执行。
6. 执行完 `done`，TUI 在 transcript 里汇总各自改动。

### 5.4 冲突与终止（复用现有引擎能力）

- **写冲突**：`claim` 直接映射到 `Coordinator::acquire_task(TaskRequest::write(path))` 的路径租约；祖先 / 子路径冲突仍被拒绝。去中心化不等于没有锁。
- **资源**：模型请求仍走 `acquire_model`，共享 provider 槽位、cooldown 与 token 预算。
- **终止**：topic 轮数上限（默认 6 轮）、房间 token 预算、用户 `Esc` 中断全部或 `Alt+数字` 中断单个代理。到上限未收敛时，停止并把分歧摘要交给用户裁决，不自动选一方。
- **持久化**：`.mio/rooms/<room-id>.jsonl` 追加写；`--no-session` 不落盘。恢复时不自动重放副作用，与现有 unknown-effect 规则一致。
- **RPC 同核**：新增 `room_message` / `room_state` 事件与 `room_post` 命令，Ravomi Mio Chat 与 TUI 读同一份状态。

### 5.5 jj 内部协同与远程交付

编码 Agent 在 Mio 自管的独立版本管理区域中工作：每个 Agent 绑定一个 jj workspace，当前任务绑定一个稳定 change ID；任务依赖映射为 change 的祖先关系。路径 `claim` 仍由 Mio 的协调器强制，不能用 jj 的冲突记录代替写入准入。

整合阶段由专门 Agent 或确定性流程读取 change 图，执行重排、合并和冲突处理；jj operation log 用于恢复版本管理操作，但不自动重放模型调用、Shell 或其他外部副作用。对外以 jj bookmark 和 `jj git fetch/push` 连接现有 Git 远程，用户与 Agent 不直接执行 Git 命令。

Mio 的版本管理界面只暴露 jj 的 change、workspace、bookmark、operation log 和同步语义。底层 Git backend 是与现有远程兼容的实现细节，不形成第二套用户工作流。

## 6. 键位

| 键 | 行为 |
| --- | --- |
| `Enter` / `Shift+Enter`（或 `Alt+Enter`） | 发送 / 换行 |
| `@` + `Tab` | 补全代理昵称、别名或文件路径 |
| `/` | 命令弹层（模糊过滤） |
| `Alt+←/→` | 切换焦点代理（composer 底色随之变化） |
| `Esc` | 运行中：中断；空闲时连按两次：回退编辑上一条 |
| `Ctrl+R` | 历史搜索 |
| `Ctrl+O` | 展开 / 折叠长工具输出与思考 |
| `Ctrl+C` | 有输入时清空，空输入时退出 |

## 7. 分阶段落地（每阶段单独验收）

- **P0 骨架**：加入 `ratatui`、`crossterm`、`unicode-width`、`unicode-segmentation`、`textwrap`、`pulldown-cmark`（本机 registry 均已缓存）；`tui/terminal.rs` + `history.rs` 跑通“历史进 scrollback + 底部固定 composer + footer”；Mocha 主题与单代理底色。非 TTY 路径行为不变。
- **P1 编辑器**：多行、bracketed paste、历史落盘、`Ctrl+R`、`/` 弹层、`@` 文件补全，替换手写 `InteractiveEditor`。
- **P2 transcript**：cell 化渲染、markdown 流式两段式提交、工具状态着色、思考折叠、diff 块。
- **P3 弹层**：审批三态（本次 / 总是 / 拒绝）、`/model`、`/thinking`、`/agents`。
- **P4 房间引擎（无头）**：`src/room.rs` + `room_post` 工具 + RPC 事件；用 mock provider 测协商收敛、版本错配拒绝、轮数上限、写租约冲突。先于 TUI 落地，保证 GUI / RPC 同样可用。
- **P5 房间 TUI**：`@A`…`@AF` 固定身份色、角色文字、多目标 composer、焦点切换、独立动态状态行和 topic 汇总视图。

验收：每阶段都要有伪终端回放测试（按键、粘贴、resize），渲染用 ratatui `TestBackend` 做快照；四项门禁（fmt / check / test / clippy）保持通过。

## 8. 已定决策与保留配置

1. **终端布局**：允许 inline viewport，禁止 alt-screen 全屏界面；保留终端 scrollback。
2. **房间原语**：房间是独立的平级协作原语，不在 `SubagentManager` 上追加 peer 模式；编码成员的工作绑定内部 jj workspace/change。
3. **身份与角色**：使用截图所示的 `@A`…`@AF` 固定字母和 32 色表区分 Agent；角色、模型和角色提示词是独立配置，不参与寻址。
4. **并发与终止**：同时活跃数沿用 coordinator 限额；topic 默认最多 6 轮。房间 token 预算保留为宿主可配置项，在固定评测后确定默认值。
5. **主题**：只提供 Catppuccin Mocha 深色主题；不实现 Latte。OSC 11 仅用于计算深色 composer 混色，查询失败回退到 Mocha Base。
