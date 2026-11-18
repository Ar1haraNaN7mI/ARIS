# ARIS Web

React + TypeScript + Vite 工作台，共用 Rust 的模型、团队、会话、审批和事件存储。主任务对话、子智能体会话、执行图和工具记录使用后端真实状态。

## 开发

```powershell
cd web
npm ci
npm run build
cd ..
cargo run -p aris-cli -- web --assets web/dist
```

默认地址为 `http://127.0.0.1:8770/`。热更新时保持 Rust 服务运行，在 `web` 目录执行 `npm run dev`，访问 `http://127.0.0.1:5173/`；Vite 代理 `/api` 至后端。`vite preview` 只提供静态资源，不能单独执行任务。

日常安装使用 `python scripts/install_aris.py`，随后从任意工作区执行 `aris web` 或 Windows 上的 `aris app`。安装资源位于 `~/.cargo/share/aris/web`。没有开屏或桌宠；`--startup` 等旧参数已移除。

## 数据流

`GET /api/harness/bootstrap` 提供工作区、会话与本机访问令牌。`POST /api/harness/rpc` 接收 `{method,params}`，请求携带 `X-ARIS-Token`。

- `teams/*`：团队、定向/整体追加消息、交接、快照和恢复。
- `sessions/*`：每个智能体的真实会话及工具调用/结果。
- `approvals/list|resolve`：待处理工具与批准结果。
- `templates/*`：内置和自建模板，绑定模型档案。
- `kei/*`：模型档案、导入状态与连接配置。

浏览器登录导入只在 URL 中传一次性 code；凭据在本机服务兑换。原生窗口把授权链接交给系统浏览器，成功后接收受同源限制的 `aris-imported` 通知并激活窗口。

## 验证

```powershell
npm test
npm run build
```

根目录 `scripts/test_aris_integration.py` 验证真实本机 HTTP 服务、固定模型并发、依赖、回滚、定向/团队续聊及 PKCE 回调；其中上游模型为明确的本地 fixture。生产模型与真实浏览器登录需要另外验收。
