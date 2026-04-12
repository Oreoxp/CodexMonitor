# CodexMonitor 功能清单

---

## 一、已拥有的 1.0 资产 (What We Have)

### 核心工作区
- **多工作区 (Workspace) 隔离管理** — 每个工作区独立运行，拥有独立的 Agent 会话、文件上下文和 Git 状态
- **Worktree 支持** — 可为每个任务创建独立的 Git Worktree，避免分支污染；支持重命名、初始化和从 URL 克隆
- **工作区启动脚本** — `useWorkspaceLaunchScript` / `useWorktreeSetupScript` 支持工作区级别的自动初始化脚本

### 对话与线程
- **多线程对话管理** (`useThreads`) — 支持多个并发 Agent 对话线程的创建、切换、归档和复制
- **消息渲染** — Markdown 渲染（GFM 支持）、代码高亮（Prism.js）、长列表虚拟化（TanStack Virtual）
- **Plan 视图** — Agent 规划阶段有专属 `plan.css` 展示面板，区分规划状态和执行状态
- **中断快捷键** — `useInterruptShortcut` 支持快速打断正在执行的 Agent

### Composer（输入区）
- **富文本编辑器状态管理** — `useComposerEditorState` 处理输入、草稿保留
- **快捷键系统** — `useComposerShortcuts` 支持键盘快捷操作
- **菜单动作** — `useComposerMenuActions` 提供输入区附加操作（附件、模式切换等）

### Git 集成
- **Diff 审批面板** — 使用 `@pierre/diffs` 渲染结构化差异，有专属 `diff.css` / `diff-viewer.css` / `ds-diff.css`
- **PR Composer** — `usePullRequestComposer` 支持直接在 UI 中起草并提交 Pull Request
- **分支切换器** — `useBranchSwitcherShortcut` / `branch-switcher-modal` 快速切换 Git 分支
- **自动退出空 Diff** — `useAutoExitEmptyDiff` 检测空变更集并自动退出审批流程

### 通知与审批流
- **Toast 通知体系** — 分层 Toast：`approval-toasts`（工具调用审批）、`error-toasts`（错误）、`update-toasts`（更新提示）
- **Request User Input 面板** — Agent 主动请求用户输入时有专属 UI 展示
- **系统通知** — 集成 `@tauri-apps/plugin-notification`，支持桌面级原生通知
- **响应必要通知** — `useResponseRequiredNotificationsController` 检测 Agent 等待状态并提醒用户

### 模型与协作模式
- **多模型支持** — `useModels` 管理可用模型列表，支持运行时切换
- **协作模式** — `useCollaborationModes` / `useCollaborationModeSelection` 支持多种 Agent 协作策略
- **Skills 系统** — `useSkills` 管理可调用的预定义技能集合
- **Apps 系统** — `useApps` 管理可集成的外部应用

### 终端与系统工具
- **内嵌 PTY 终端** — `@xterm/xterm` 提供全功能嵌入式终端，支持自适应大小（`addon-fit`）
- **调试视图** — 独立 `debug.css` 面板，用于查看 Agent 原始协议消息
- **文件树视图** — `file-tree.css` 文件浏览面板

### 账户与设置
- **账户切换** — `useAccountSwitching` / `useHomeAccount` 支持多账户登录态管理
- **设置面板** — `settings.css` 全局设置 UI
- **关于页面** — 独立 `about` 窗口，懒加载

### 移动端 / 远程支持
- **移动端远程工作区** — `useMobileServerSetup`，支持从移动设备远程连接到本地 App-Server
- **响应式布局** — `compact-base.css` / `compact-phone.css` / `compact-tablet.css` 多尺寸适配

### 自动更新
- **应用内更新** — `@tauri-apps/plugin-updater` 集成，支持后台检测并提示更新

---

## 二、2.0 功能缺口 (Gap Analysis)

以下是对照"私人数字公司"四大支柱，当前前端**完全缺失**或**严重不足**的核心模块：

### 缺口 1：统一 Auth 网关接入（Unified Auth & Cost Control）
**现状：** UI 层完全没有统一认证入口。当前账户切换（`useAccountSwitching`）的本质是直接切换本地配置，而非接入一个统一的自建 Auth 网关。

**缺失内容：**
- 自建 Auth 网关的登录/登出 UI 流程（OAuth、Token 登录等）
- API 费用看板 —— 展示各 Agent 的 Token 消耗、费用统计、配额警告
- 多 Agent 共享 API Key 的集中管理界面，Boss 能看到每个"员工"花了多少钱
- 费用超限时的 UI 拦截和提示

---

### 缺口 2：Dream / Memory 系统可视化（长期记忆）
**现状：** 完全无任何记忆系统的 UI 入口。对话线程间彼此孤立，Agent 没有跨会话的长期上下文。

**缺失内容：**
- **Memory 面板** —— 可视化展示当前 Agent 的记忆条目（类似 CLAUDE.md 的 memory 体系）
- **记忆审批流** —— 当 Agent 尝试写入/修改记忆时，触发 Boss 审批 Toast
- **Dream 压缩视图** —— 长对话结束后展示"本次会话摘要"，可确认是否存入长期记忆
- **记忆检索 UI** —— 允许用户手动搜索、编辑、删除 Agent 的历史记忆条目

---

### 缺口 3：KAIROS 后台主动监控系统
**现状：** 完全缺失。所有 Agent 行为都是被动响应用户输入，没有任何主动监控和事件驱动的 Agent 触发机制。

**缺失内容：**
- **KAIROS 监控面板** —— 展示当前后台监听的任务列表（文件变更、定时触发、外部 Webhook 等）
- **事件日志流** —— 实时滚动显示 KAIROS 检测到的事件
- **主动提醒 UI** —— KAIROS 发现异常（构建失败、PR 评论、监控告警）时主动弹出通知，无需用户询问
- **监控规则编辑器** —— Boss 可配置"监听什么条件 → 触发哪个 Agent 做什么动作"

---

### 缺口 4：Buddy / 性格系统（Agent 人格与角色）
**现状：** 完全缺失。所有 Agent 在 UI 层面没有身份区分，仅显示为匿名对话线程。

**缺失内容：**
- **Buddy 卡片** —— 每个 Agent 拥有名字、头像、角色描述（如：「代码审查官」、「文档专员」）
- **性格配置面板** —— Boss 可为每个 Agent 设定沟通风格（严谨/活泼/简洁）、专业领域、响应语气
- **Buddy 选择器** —— 发起新任务时，可选择"派遣"哪位员工来处理
- **员工工作状态** —— 显示各 Buddy 当前是空闲、执行中还是等待审批
- **对话个性化** —— 消息气泡展示 Buddy 身份标识，而非统一的匿名机器人样式

---

### 缺口 5：跨 Agent 任务编排（补充愿景）
**现状：** 每个 Workspace 只能运行单一 Agent 会话，多 Agent 之间没有协作编排 UI。

**缺失内容：**
- **任务派发面板** —— Boss 发布一个目标，系统自动拆分并分发给多个 Buddy
- **Agent 间消息传递可视化** —— 展示 Agent A 将结果交给 Agent B 的中间状态
- **并行任务看板** —— 类 Kanban 视图，展示所有 Agent 当前任务的进度
