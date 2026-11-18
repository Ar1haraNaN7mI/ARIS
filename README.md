# ARIS

<img src="docs/assets/aris-emblem.svg" alt="ARIS" width="80" />

面向研究、写作、数据分析、运营、编程和浏览器工作的多智能体 Harness。Rust 运行时、终端 CLI、React 工作台与 Windows 原生窗口共用一套任务和会话状态。

## 工作方式

- **自由组队**：1–32 个智能体；角色、任务、固定模型档案、依赖与交付文件均可编辑，每波最多 8 个并行。
- **对话工作台**：输入目标，由已配置模型规划 2–8 个协作角色后执行；按任务分组的子会话、顶部协作顺序和右侧工作树同时可见。直接与子智能体续聊，运行中的补充消息会排队，在当前执行阶段结束后消费，保留原会话。
- **模板**：内置调研、竞品、内容、数据、运营、工程实现、审查、文档、网页自动化和故障排查 10 类。可以修改、另存、自建和删除个人模板。
- **模型隔离**：每次运行固定客户端与提示词副本。不同智能体可分别使用 DeepSeek、Grok、Gemini、GPT/Codex、GLM、Kimi 家族的模型；真实可用范围由 KEI 账户决定。
- **自动衔接**：独立会话共用工作区文件，后续阶段收到依赖结果后自动继续，无需手动切换执行者。命令行仍提供显式交接与下游重跑能力。
- **真实回滚**：声明的交付文件在运行前后保存快照。恢复前先保存撤销快照，不修改用户 Git index 或 HEAD。
- **独立浏览器**：每个智能体独立 Playwright context、页面、Cookie 和页面鼠标。支持截图、导航、点击、输入、按键、滚动和拖拽。
- **网页获取**：`web_crawl` 集成 Scrapling，返回带来源 URL、时间和 HTTP 状态的 Markdown；支持静态抓取与显式动态渲染。

没有开屏动画或桌宠。原有 Windows UIA/OCR/桌面指针实现已经移除。浏览器鼠标是页面级控制，不是多个物理系统光标；原生桌面应用自动化不在当前驱动范围内。

## 安装

需要 Rust stable、Python 3.10+、Node.js 22.12+、npm。建议使用仓库锁定依赖。

```powershell
python scripts/install_aris.py
aris web
# Windows 原生窗口
aris app
```

安装器将 CLI、网页、浏览器运行时和 Scrapling 环境安装至 `~/.cargo`，可从任意工作目录使用。`--debug` 使用开发构建；`--skip-tool-runtimes` 只安装 CLI 和网页。动态 Scrapling 抓取还需要其浏览器依赖，见 [Scrapling 适配器](integrations/scrapling/README.md)。

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

```powershell
aris                         # 终端交互
aris exec "总结当前目录材料"   # 单次任务
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
