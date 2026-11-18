# ARIS 运行时与接口

## 模块

`aris-cli` 提供终端、JSON-RPC、同源本机 HTTP 和桌面入口；`aris-core` 负责会话、任务、模型档案、团队依赖执行、文件快照和工具调用；`aris-llm` 统一 Chat Completions 与 Responses。React 工作台调用相同 RPC，界面不自行模拟执行状态。

`AgentLoop` 在每次运行开始时复制模型配置和提示词。配置修改只能影响后续运行。工具上下文的 `agent_id` 使用持久会话 ID，浏览器按此身份隔离。Responses 的原始 reasoning/function-call 项保留用于后续工具循环，Chat Completions 请求不会混入这些协议字段。

每轮 `turn_start` 记录当时实际使用的 `profile_id`、`model_id` 和 `family`。聊天与工作树据此显示历史模型身份；后续换模不会改变过去的标记。旧会话缺少这些字段时保留记录并显示中性图标。

## 团队状态

每个工作流持久化于工作区 `.aris/teams/<uuid>/team.json`，包含成员、依赖、固定模型档案、会话 ID、结果、交接、事件和快照索引。

手动启动从 `idle` 进入 `running`。通过 `teams/admin` 发送消息时，工作台立即收到状态为 `planning` 的团队。管理员直接作答后，回答保存在同一个会话。需要派工时，服务持有同一把运行锁，将通过校验的依赖图交给执行器。团队的 `admin` 字段记录模型、会话、回答和阶段。自动派工每轮最多执行六个角色，最多三个并行。手动工作流每波最多八个。两种路径都要等待前置结果和文件租约。

运行以 `completed`、`failed` 或 `cancelled` 收尾。取消先持久化 `cancelling`，再通知运行中的智能体并等待工具停止。进程中断后，恢复读取会识别失效的运行租约。

每个工作流使用独占运行锁；状态读改写使用独立文件锁，避免跨进程取消被覆盖。声明的文件使用工作区级租约，阻止其他工作流同时修改或恢复相同文件。文件声明是编排约定，不能替代操作系统沙盒。

回滚只修改声明文件；禁止路径越界、链接跳转、私有目录和含歧义的平台文件名。运行前后与手动检查点都保存实际内容，恢复前再保存一份撤销快照。会话历史与用户 Git index/HEAD 不回滚。

## RPC

- `teams/list`, `teams/get`, `teams/create`, `teams/admin`, `teams/plan`, `teams/model`, `teams/run`, `teams/cancel`
- `teams/message`, `teams/handoff`, `teams/checkpoint`, `teams/restore`
- `templates/list`, `templates/save`, `templates/delete`
- `kei/config`, `kei/update`, `kei/credential`, `kei/models`
- `kei/import/start`, `kei/import/status`
- `computer/status`, `computer/setEnabled`, `computer/sessions`, `computer/action`

已有 `sessions/*`、`tasks/*`、`events/*`、`approvals/*` 与工具、扩展管理仍由共同运行时提供。

`teams/message` 接收 `id`、`message` 和可选 `agent_id`。团队消息启动一轮完整协作；定向聊天只继续选中智能体的原会话，不重跑已完成的下游。执行中的追加消息会持久化排队，在当前执行波结束后应用；取消时保留待处理消息，下一次继续再消费。同一智能体不会因追加消息产生重叠模型循环。运行中的回复通过内存快照返回给工作台，完成后的全文保存在会话与团队记录。CLI 的 team message 则保留定向重跑下游语义。

## KEI 一键导入

ARIS 创建随机 verifier、S256 challenge、state 和固定的本机回调 URI。登录与模型/Key 选择发生在 KEI 页面；确认后 KEI 只把短时一次性 code 放入回调 URL。

本机回调验证 state 和有效期，向同一 KEI origin 的 `/api/aris/import/redeem` 发送 code/verifier/redirect_uri。KEI 原子消费 code 并重新检查用户、Key 与模型权限，再返回模型目录和所选 Key。ARIS 检查返回 origin，写入本机私有凭据引用和模型档案。导入失败不会被显示为成功。

## 浏览器与网页获取

`integrations/browser-runtime` 是持久 Node/Playwright JSONL 子进程。每个智能体独立 BrowserContext、页面集、Mouse 与队列；同一智能体操作有序，不同智能体可并行。图像保存至用户目录 `computer/screenshots`，仅允许此目录内的有效截图成为模型图像输入。退出时等待驱动释放浏览器资源。

`integrations/scrapling` 是固定依赖版本的 Python 适配器；`web_crawl` 通过有界子进程输入输出调用。静态和动态模式均返回来源、抓取时间、正文、截断和错误信息。依赖缺失时返回安装提示，不把空结果伪装为成功。
