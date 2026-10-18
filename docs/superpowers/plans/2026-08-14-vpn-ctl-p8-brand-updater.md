# P8 实施计划：产品化（云驿品牌 + tauri-updater 自动更新）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用户可见面以「云驿」品牌呈现；安装包具备经签名的自动更新能力（GitHub Releases 分发）。

**Architecture:** 品牌面 = tauri.conf.json（ASCII productName `YunYi` + 中文窗口标题）+ UI 文案替换；更新面 = tauri-plugin-updater（Rust 注册 + capabilities + 签名密钥对）+ 前端「检查更新」入口 + release.yml 流水线。

**Tech Stack:** Tauri 2（配置与插件形态已按官方文档 v2.tauri.app/plugin/updater 核实，2026-08-14）、React 19、GitHub Actions。

**规格：** `docs/superpowers/specs/2026-08-14-vpn-ctl-p8-brand-updater-design.md`

## Global Constraints

- 无 emoji（UI 铁律）；更新失败可读中文原因；凭据（签名私钥）不进日志/提交/错误文本。
- 内部路径/目录/identifier 保持中性（com.vpnctl.app、vpn-ctl.yaml 等不动）；品牌名仅用户可见面。
- 修改公共导出/签名前 rg 搜索引用并同步；每个功能有测试。
- 官方文档核实过的配置形态（2026-08-14）：
  - `bundle.createUpdaterArtifacts: true`；`plugins.updater.pubkey`（**公钥内容字符串，非路径**）+ `endpoints`
  - 密钥生成：`npm run tauri signer generate -- -w <路径>`；私钥不提交，公钥入配置
  - latest.json：`{version, notes, pub_date, platforms: {"windows-x86_64": {signature, url}}}`
  - Rust 注册：`tauri_plugin_updater::Builder::new().build()`；JS：`check()` → `{available, version, body}` → `downloadAndInstall()`；capabilities：`updater:default`
- 发布门禁：仅 master 触发；「构建成功」≠「发布完成」——必须真实安装验证。

---

### Task 1: 品牌落地

**Files:**
- Modify: `src-tauri/tauri.conf.json`（productName `vpn-ctl`→`YunYi`、version `0.1.0`→`0.2.0`、windows title「云驿」）
- Modify: 前端所有显示产品名处（rg 定位「vpn-ctl」的 UI 文案——Settings/App 标题等，实施时逐个替换为「云驿」；代码内命令名/标识符不动）
- Test: 前端相关组件测试同步（文案断言）

- [ ] Step 1: rg 全仓库定位 UI 可见的 vpn-ctl 文案（排除命令名/路径/文档）
- [ ] Step 2: 替换 + 测试断言同步
- [ ] Step 3: `cargo test` + `vitest` + `tsc` 全绿；dev 启动验证窗口标题「云驿」
- [ ] Commit `feat(p8): 云驿品牌落地（productName/窗口标题/UI 文案）`

### Task 2: updater 依赖、配置与密钥

**Files:**
- Modify: `src-tauri/Cargo.toml`（+`tauri-plugin-updater = "2"`）
- Modify: `package.json`（+`@tauri-apps/plugin-updater`）
- Modify: `src-tauri/src/main.rs` 或 lib（插件注册——按项目现状入口定）
- Modify: `src-tauri/capabilities/*.json`（+`updater:default`）
- Modify: `src-tauri/tauri.conf.json`（bundle.createUpdaterArtifacts + plugins.updater.pubkey/endpoints——endpoints: `https://github.com/cnmlgbdrnm-prog/vpn-ctl/releases/latest/download/latest.json`）
- Modify: `.gitignore`（+私钥文件模式）

- [ ] Step 1: `npm run tauri signer generate -- -w .signing/yunyi-updater.key`（私钥文件落本地不提交；**公钥输出抄入配置，私钥内容不进任何日志**）
- [ ] Step 2: 配置写入；`gh secret set TAURI_SIGNING_PRIVATE_KEY --repo cnmlgbdrnm-prog/vpn-ctl`（stdin 读私钥文件，不打印）
- [ ] Step 3: `cargo test` + `cargo build` 验证插件注册编译通过
- [ ] Commit `feat(p8): updater 插件接入与签名公钥配置`

### Task 3: 前端「检查更新」

**Files:**
- Modify: `src/views/Settings.tsx`（「检查更新」按钮 + 结果展示：已最新/发现新版本+notes/下载安装中/失败原因）
- Modify: `src/api/index.ts`（updater 封装：checkAndUpdate()）
- Test: `src/views/Settings.test.tsx`（mock @tauri-apps/plugin-updater：available true/false/失败三路径）

- [ ] Step 1: 写失败测试（三路径 + 文案断言）
- [ ] Step 2: 实现（update.available → 展示 version+body → 确认下载安装；无更新「已是最新版本」；失败可读中文）
- [ ] Step 3: vitest + tsc 全绿
- [ ] Commit `feat(p8): 设置页检查更新入口`

### Task 4: release.yml 与发布文档

**Files:**
- Create: `.github/workflows/release.yml`（workflow_dispatch + version 输入；windows-latest：fetch:sidecar → tauri-apps/tauri-action@v0（env TAURI_SIGNING_PRIVATE_KEY）→ 上传 release assets + latest.json（signature 从构建产物 .sig 读，url 指向 release 资产）→ release notes 从 CHANGELOG）
- Create: `CHANGELOG.md`（0.2.0 起始）
- Modify: `README.md`（品牌、更新说明）

- [ ] Step 1: 写 release.yml（steps 注释说明签名与 latest.json 生成逻辑）
- [ ] Step 2: CHANGELOG/README
- [ ] Step 3: YAML 语法人工核对（actionlint 无——按 CI 同策略，触发后观察）
- [ ] Commit `ci(p8): release 流水线（签名+latest.json）与 CHANGELOG`

### Task 5: 真实发布 0.2.0 验收（R4）

- [ ] 触发 release.yml（version 0.2.0）→ 构建全绿 → Releases 有安装包 + .sig + latest.json
- [ ] 下载安装 0.2.0 真实验证（窗口标题云驿、功能冒烟）
- [ ] 「检查更新」路径：已是最新（真实端点）
- [ ] 更新链路验证：手动构造 0.2.1 假 latest.json 场景验证发现+签名校验失败路径（或按可行性：先发布 0.2.0 后本地改版本号构建 0.2.1 完整走一遍——以实际时间为准，至少验证「已是最新」与「签名不匹配被拒」两路径）
- [ ] Commit 验收记录

---

## 验收

1. CI + 本地全绿；窗口标题与 UI 文案为云驿；内部路径未动
2. Releases 有经签名的 0.2.0 安装包；latest.json 合法（字段与官方格式一致）
3. 更新检查真实端点可达（已是最新）；失败路径可读中文
