# ARIS

<img src="docs/assets/aris-emblem.svg" alt="ARIS" width="80" />

面向研究、写作、数据分析、运营、编程和浏览器工作的多智能体 Harness。Rust 运行时、终端 CLI、React 工作台与 Windows 原生窗口共用一套任务和会话状态。

## 工作方式

- **自由组队**：1–32 个智能体；角色、任务、固定模型档案、依赖与交付文件均可编辑，每波最多 8 个并行。
- **管理员与子智能体**。空白会话由管理员接收。它可以直接回答、调用 `create_sub_agents` 创建并分派子智能体，或用 `dispatch_sub_agents` 派给已有成员。每个子智能体都有固定模型、独立会话和可查询的任务与运行记录。两份独立交付可以并行完成，需要合写一份报告时再安排汇总。
- **对话工作台**。管理员和每个子智能体在顶部各占一行，在右侧工作树各占一列。点击创建结果中的名字或泳道列头，可以查看该智能体的对话、交付并单独追问。跨列连线显示分工、交接与依赖。任务的开始、结束和工作区快照收在「团队记录」。
- **模板**：内置调研、竞品、内容、数据、运营、工程实现、审查、文档、网页自动化和故障排查 10 类。可以修改、另存、自建和删除个人模板。
- **模型隔离**：每次运行固定客户端与提示词副本。不同智能体可分别使用 DeepSeek、Grok、Gemini、GPT/Codex、GLM、Kimi 家族的模型；真实可用范围由 KEI 账户决定。
- **自动衔接**：独立会话共用工作区文件，后续阶段收到依赖结果后自动继续，无需手动切换执行者。命令行仍提供显式交接与下游重跑能力。
- **真实回滚**：声明的交付文件在运行前后保存快照。恢复前先保存撤销快照，不修改用户 Git index 或 HEAD。
- **独立浏览器**：每个智能体独立 Playwright context、页面、Cookie 和页面鼠标。支持截图、导航、点击、输入、按键、滚动和拖拽。
- **网页获取**：`web_crawl` 集成 Scrapling，返回带来源 URL、时间和 HTTP 状态的 Markdown；支持静态抓取与显式动态渲染。

没有开屏动画或桌宠。原有 Windows UIA/OCR/桌面指针实现已经移除。浏览器鼠标是页面级控制，不是多个物理系统光标；原生桌面应用自动化不在当前驱动范围内。

## 安装

完整安装需要 Rust stable、Python 3.11+、Node.js 22.12+ 和 npm。剪辑视频还需要 FFmpeg 与 ffprobe。

```powershell
python scripts/install_aris.py
aris web
# Windows 原生窗口
aris app
```

安装器将 CLI、桌面程序、网页、内置技能和工具运行时安装至 `~/.cargo`，可从任意工作目录使用。`--debug` 使用开发构建。`--skip-tool-runtimes` 保留程序、网页和技能，跳过浏览器、Scrapling 和媒体依赖。动态抓取需要额外的浏览器依赖。[Scrapling 适配器](integrations/scrapling/README.md)

开发运行：

```powershell
cd web
npm ci
npm run build
cd ..
cargo run -p aris-cli -- web --no-open
```

## KEI 模型导入

在设置中点击 **从 KEIAPI 一键导入**：

1. 跳转 KEI 登录，登录后继续原导入请求。
2. 选择要导入的模型家族。
3. 选择已有 API Key 或在同页新建，再确认该 Key 实际可访问的精确模型。
4. 浏览器返回本机 ARIS，模型档案与凭据保存成功后即可分配给智能体。

当前模型由 KEI 动态返回，不内置虚构模型 ID。导入需要 KEI 服务端同时部署 ARIS 导入接口。浏览器 URL 只含短时一次性授权码和 state；Key 通过 PKCE 校验后的服务端兑换返回，保存于本机 `~/.aris`，不写入项目文件或 URL。每次导入使用独立的凭据引用，后续导入不同 Key 不会替换已有档案的 Key。

也支持手动配置：

```powershell
$env:KEI_BASE_URL = "https://kei-code.com/v1"
$env:KEI_API_KEY = "你的 API Key"
aris models
```

在设置里为档案填写返回的精确 ID，并选择 `chat_completions` 或 `responses`。GPT/Codex 在这里指通过 KEI 调用的模型，不是托管 Codex CLI 订阅登录。模型是否支持工具、图像及对应协议，取决于实际通道。

## CLI 工作流

工作台的新对话可以直接输入目标。管理员先判断当前信息是否足以回答。需要操作文件或生成媒体时，它可以派给一位执行者。遇到能独立开展的研究和核算，它会并行派工，再把结果交给负责汇总的角色。自建团队保留原成员、会话和固定模型。

两位执行者可以在各自泳道之间开启临时协商。点击连线可以查看双方共享的对话。「已采纳」等通知只记入泳道。提问、待确认的提议和协作邀请可以唤醒相应成员。收到 co-work 邀请的成员可以接下或婉拒，接下后在自己的会话里执行，并把结果或文件位置交回通道。发起人需要拿到结果继续加工时，可以在发出请求时声明。

需要合稿时，管理员指定一位最终作者。自建团队也可以在「最终汇总」中选择作者。打开「最终交付」即可阅读和复制本轮成稿，切换到上游研究文件，或回看影响结论的协商记录。

管理员优先使用已配置的 `gpt-6.2-astra`。也可通过 `--profile` 指定其他已导入的模型档案。推理档位沿用该档案和协议的显式设置。管理员会直接回答简单问题。需要派工时，它按可独立完成的工作分配角色，优先复用已有成员。首次建队和后续补员共用八个子智能体名额，管理员单独计数。一轮可把任务派给最多八位执行者，同时最多三位工作，其余就绪角色分批运行。普通执行限时 15 分钟。需要整合至少两位上游成员交付的主笔，只要在这段时间内启动，从首次开工起至少有 8 分钟完成成稿。

```powershell
aris                         # 终端交互
aris exec "总结当前目录材料"   # 单次任务
aris team admin "解释七日激活率的统计口径"
aris team admin "两人分别核算成本与转化，再合成决策简报"
aris team admin "更新上一轮的成本估算" --id <team-id> --existing-roles-only
aris team plan "调研目标用户，比较方案并形成报告" --run
aris team create workflow.json
aris team list
aris team run <team-id>       # 等待完成，失败返回非零状态
aris team get <team-id>
aris team checkpoint <team-id> "已核对的版本"
aris team restore <team-id> <checkpoint-id>
aris team handoff <team-id> researcher writer "根据研究结果完善交付"
aris team message <team-id> "补充成本与限制"         # 新一轮团队协作
aris team message <team-id> "复核数据单位" --agent researcher
```

在对话里也可以直接要求剪辑素材、制作中文海报或生成插画。可用格式、图像模型配置和命令示例见[媒体工具说明](docs/media-tools.md)。

普通会话、管理员和执行者都使用内置写作指导。报告可调用 `report_calculate` 批量复核加权分数、比率和预算。安装包附带 Anti-Defensive Writing 的中英文原文和 MIT 许可。[技能及来源](outer/bundled-skills/anti-defensive-writing/SKILL.md)

可编辑的工作流文件示例：

```json
{
  "name": "主题研究",
  "goal": "调查指定主题，形成有引用的报告",
  "files": ["artifacts/sources.md", "artifacts/report.md"],
  "agents": [
    {
      "id": "researcher",
      "name": "资料研究",
      "role": "收集可复查的一手证据",
      "profile_id": "gemini",
      "prompt": "阅读已有材料，抓取相关网页，记录原始 URL 与时间",
      "depends_on": [],
      "files": ["artifacts/sources.md"]
    },
    {
      "id": "writer",
      "name": "报告交付",
      "role": "核对事实并形成结论",
      "profile_id": "gpt",
      "prompt": "读取来源文件，区分事实、推断与未知项，完成报告",
      "depends_on": ["researcher"],
      "files": ["artifacts/report.md"]
    }
  ]
}
```

`profile_id` 必须是已配置的模型档案。内置 `gpt`、`gemini` 等家族档案只是可编辑入口；未填写真实模型时不能运行。

## 持久化与边界

用户模型、Key、会话和个人模板保存在 `~/.aris`（可用 `ARIS_HOME` 修改）；工作流、事件和快照保存在工作区 `.aris/teams`。

进程重启后会把未结束运行标为中断，保留已完成结果；重新执行只运行尚未完成的阶段。同一文件的声明写入会串行，跨工作流使用文件租约。请准确声明交付文件：租约与回滚仅覆盖这些文件，不对任意 shell 命令提供文件级隔离。每个工作流最多 128 个快照文件、每个快照最多 64 MiB。

Scrapling 默认同源抓取，最多 10 页、3 层、60 秒、60,000 字符；返回截断标记与逐页错误。它是网页内容获取工具，不是搜索引擎。

## 验证

```powershell
cargo test --workspace
cd web
npm test
npm run build
cd ../integrations/browser-runtime
npm ci
npm test
cd ../scrapling
python -m unittest test_crawl.py
```

真实 Rust/浏览器桥接测试需先安装浏览器依赖：

```powershell
cargo test -p aris-core real_browser_bridge -- --ignored
```

本地模型 HTTP fixture 验证协议、并发和失败分支，不代表所有上游模型均已在线验证。

界面采用天青色主题和原创几何 ARIS 标志。光线渲染复用 [Lancer Light UI](https://github.com/raylee-ai/lancer/tree/main/packages/light-ui) 的 MIT 源码，许可证与出处随 `web/src/vendor/lancer-light-ui` 保留；运行数据来自 ARIS 的实际会话与工具事件。ARIS 底层使用独立 Rust 运行时。

架构详见 [运行时与接口](docs/architecture.md)。协作交接的参考来自 [Lancer 作者工作流说明](https://github.com/raylee-ai/lancer/blob/main/docs/lancer-conversation-workflows.md)；浏览器实现采用 [OpenAI 公开 computer-use 集成方式](https://developers.openai.com/api/docs/guides/tools-computer-use-integration)与 [Playwright BrowserContext](https://playwright.dev/docs/browser-contexts)，没有依赖 ChatGPT 私有接口。
