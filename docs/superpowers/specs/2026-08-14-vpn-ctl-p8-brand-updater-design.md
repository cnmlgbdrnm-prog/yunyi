# vpn-ctl P8 设计规格：产品化（云驿品牌落地 + tauri-updater 自动更新）

> 状态：待老板审阅。战略见 `docs/product/strategy.md`。品牌决策：**云驿（YunYi）**；签名私钥决策：**GitHub Secrets 自动签名**（均老板已拍板）。

## 1. 背景与目标

- 现状：productName/版本均为 `vpn-ctl` / `0.1.0`，无 updater 插件，无任何发布流水线；UI 无品牌名
- 目标：用户可见面以「云驿」品牌呈现；安装包具备自动更新能力（GitHub Releases 分发 + 签名校验）

## 2. 品牌落地（铁律约束）

内部路径/目录/临时文件/仓库名维持中性命名（`vpn-ctl`、`com.vpnctl.app`、`data/` 等全部不动）；品牌名仅用于用户可见面：

| 面 | 改动 |
|---|---|
| 窗口标题 | `tauri.conf.json` windows title → 「云驿」 |
| UI 文案 | 标题栏/关于/侧边栏等处产品名显示 → 「云驿」（实施时按现状定位全部显示点） |
| productName | 保持 ASCII：`YunYi`（Windows 安装器/注册表对非 ASCII productName 有已知坑；显示名由窗口标题承担） |
| identifier | 保持 `com.vpnctl.app`（中性，不变） |
| 版本 | `0.1.0` → `0.2.0`（semver，自动更新的首个可分发版本） |

范围外：应用图标（老板未要求，不做设计）；内部路径改名（铁律禁止）。

## 3. tauri-updater 自动更新

### 3.1 依赖与注册

- `src-tauri/Cargo.toml`：`tauri-plugin-updater = "2"`
- 前端：`@tauri-apps/plugin-updater`（npm）
- `src-tauri/src/lib.rs`（或 main 注册处）：`.plugin(tauri_plugin_updater::Builder::new().build())`
- capabilities：`updater:default` 权限

### 3.2 签名密钥

- 本地生成：`npm run tauri signer generate -w <私钥文件>`（私钥**不提交**，加 .gitignore）
- 公钥写入 `tauri.conf.json` → `plugins.updater.pubkey`
- 私钥内容存 GitHub Secrets：`TAURI_SIGNING_PRIVATE_KEY`（Release 构建时自动签名）
- `createUpdaterArtifacts: true`（v2 配置在 `bundle` 或 updater 插件配置处，实施时按 Tauri 2 现行文档核对）

### 3.3 更新源

- endpoints：`https://github.com/cnmlgbdrnm-prog/vpn-ctl/releases/latest/download/latest.json`
- `latest.json` 由 release 流水线生成：`{ version, notes, pub_date, platforms: { windows-x86_64: { signature, url } } }`

### 3.4 前端交互

- 关于/设置视图「检查更新」按钮：`check()` → 有更新提示版本与 notes → 确认后 `downloadAndInstall()`
- 失败可读中文原因（不静默）；UI 无 emoji（铁律）

### 3.5 release.yml（GitHub Actions，手动触发）

- `workflow_dispatch` 触发（版本号输入）
- windows-latest：`fetch:sidecar` → `tauri-apps/tauri-action@v0` 构建（NSIS/MSI，env `TAURI_SIGNING_PRIVATE_KEY` 来自 Secrets）→ 上传 release assets（安装包 + 各产物 .sig）→ 生成并上传 `latest.json`（release notes 从 CHANGELOG 取）
- 复用 CI 的 rust-cache
- 发布门禁：仅 master 分支可触发；构建产物 sha256 记录在 release notes

## 4. 变更面清单

- `src-tauri/tauri.conf.json`：productName/version/window title/updater 插件配置
- `src-tauri/Cargo.toml`：+updater 依赖
- `src-tauri/src/lib.rs` 或 main：插件注册
- `src-tauri/capabilities/*.json`：updater 权限
- 前端：更新检查组件/关于视图 + 品牌文案替换
- `.github/workflows/release.yml`：新建
- `.gitignore`：+私钥文件
- 文档：README（品牌、更新说明）、CHANGELOG 起始、release notes 流程

## 5. 测试与验收

1. **门禁**：cargo test / clippy / fmt + vitest / tsc 全绿（CI 自动）
2. **品牌**：dev 启动窗口标题「云驿」；UI 无残留 vpn-ctl 用户可见文案（内部命令/路径除外）
3. **更新链路**：
   - 本地构建产物带 `.sig`（`createUpdaterArtifacts` 生效）
   - `release.yml` 触发一次真实发布 0.2.0 → GitHub Releases 有安装包 + latest.json
   - 本地装 0.2.0 后手动构造 0.2.1 场景验证「检查更新」发现新版本（或 first-release 时验证「已是最新」路径 + 假更新 JSON 验证发现路径——实施时按可行性定）
4. **失败路径**：无网络/JSON 非法 → 可读错误

## 6. 风险

- **Tauri 2 updater 配置细节**（pubkey 位置、createUpdaterArtifacts 归属、signer 命令形态）：按 Tauri 2 官方文档与本地验证为准，不凭记忆（铁律）
- **非 ASCII productName 坑**：已规避（productName 用 ASCII YunYi，窗口标题用中文）
- **首次真实发布**：Release 构建 10-20 分钟（Windows + cargo 全量），发布后必须真实安装验证（不把「构建成功」当「发布完成」）
