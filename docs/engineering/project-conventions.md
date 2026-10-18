# 项目组织规范

> 本文件是仓库组织规则的事实源。可机器检查的部分由 `scripts/check-organization.mjs`
> 执行（本地 `npm run check:org`，CI 同名校验），**任一违规不可合并**。
> 无法自动检查的条目（如命名语义、复用判断）同样是评审门槛。

## 1. 仓库分层

| 位置 | 职责 | 约束 |
|---|---|---|
| 根目录 | 仓库级配置、`README.md`、`CHANGELOG.md`、`LICENSE` | 不得新增其它文档；新文档进 `docs/<分类>/` |
| `src/` | React 前端 | 顶层只允许 `App.tsx`、`main.tsx`、`styles.css`、`vite-env.d.ts` |
| `src-tauri/src/` | Rust 后端 | 按领域分模块；IPC 壳与业务分离；单文件 ≤1500 行 |
| `scripts/` | 随仓库分发的构建/检查脚本 | 必须可重复执行、失败退出非 0；检查脚本纳入 CI |
| `tests/` | 本地测试源码（前端 + Rust） | **不随仓库上传**（`.gitignore`）；目录镜像源码结构 |
| `docs/` | 所有项目文档 | 必须落在 `docs/<分类>/` 下，禁止裸放 |

根目录只承担「让仓库能被识别与构建」的职责，不承担文档仓库职责。

## 2. 文档落位（CI 强制）

### 2.1 分类

| 分类目录 | 放什么 | 示例 |
|---|---|---|
| `docs/product/` | 产品定位、路线图、需求背景 | `strategy.md` |
| `docs/engineering/` | 工程规范、流程、协作约定 | 本文件 |
| `docs/design/` | 架构与技术设计（非计划文档） | `net-runtime-design.md` |
| `docs/guides/` | 使用、部署、运维指南 | `release-guide.md` |
| `docs/audits/` | 审计、复盘、问题调查报告 | `audit-tor-2026-09-22.html` |
| `docs/superpowers/` | Superpowers 流程产出的 `plans/` 与 `specs/` 归档 | `plans/2026-08-13-vpn-ctl-p1.md` |
| `docs/archive/` | 已过期但需保留的历史文档 | `old-roadmap.md` |

- **禁止裸放**：仓库任意位置的 `.md` / `.rst` / `.adoc` 必须位于
  `docs/<上表分类>/…`；`docs/` 根目录不得直接放文件。
- **根目录例外**只有 `README.md` 与 `CHANGELOG.md`；`.github/` 下的
  Issue/PR 模板是 GitHub 规定位置，不受此限。
- **HTML 文档**（如审计报告）同样进分类目录；根目录 `index.html` 是应用入口，
  不属于文档。
- **命名**：文件名（去扩展名）使用 ASCII 小写 kebab-case，允许字母、数字、
  点、下划线、连字符。时间相关文档以 `YYYY-MM-DD-` 前缀开头。
- **新增分类**：必须同时修改 `scripts/check-organization.mjs` 的
  `DOC_CATEGORIES` 与本文件表格，否则 CI 拒绝。

### 2.2 链接

文档内引用其它文档一律使用**仓库根相对路径**（如
`docs/product/strategy.md`），便于跨目录移动时统一替换与被 CI 检索。

## 3. 前端落位与复用

| 位置 | 放什么 |
|---|---|
| `src/views/` | 页面级组件（路由/导航可达） |
| `src/views/<页面>/` | **仅该页面使用**的私有子组件（如 `views/nodes/NodeRow.tsx`） |
| `src/components/` | 跨页面复用的共享 UI（按钮、表单、列表行、面板等） |
| `src/api/` | IPC 调用与线格式类型；页面不得自行拼接 `invoke` |
| `src/jobs/` | 后台任务注册、进度与终态处理 |
| `src/utils/` | 无 UI 的纯工具函数 |

强制规则：

- **页面不得互相引用**：`src/views/*.tsx` 不能 import 另一个顶层页面。
  需要复用的 UI 必须提取到 `src/components/`（CI 检查）。
- **同一控件被第二个页面使用时必须晋升**：从 `views/<页面>/` 移到
  `src/components/`，禁止复制重写（评审硬要求）。
- `src/` 顶层只允许四个入口文件（CI 检查），新文件按上表落位。

## 4. Rust 落位

| 位置 | 职责 |
|---|---|
| `commands/ipc.rs` | Tauri IPC 壳：只做 `State` 提取与转调，**不写业务** |
| `commands/mod.rs` | 命令层业务函数，保持薄；领域逻辑下沉到对应模块 |
| 领域模块（`net/`、`servers/`、`relay/`、`speed/` …） | 业务实现与领域类型 |
| `*_spec.rs`（如 `servers/deploy_spec.rs`） | 纯规格：协议白名单、命令/配置生成、远端配置解析 |

强制规则：

- 单个 `.rs` 文件不得超过 1500 行（`npm run check:size`）。
- 拆分模块时用 `pub use` 重导出保持原有路径（如
  `crate::servers::deploy::*`），避免测试与调用点跟着改。
- 纯函数与 I/O 编排分离：解析/生成放 `*_spec`，SSH/文件执行留在编排模块。

## 5. 资产、生成物与敏感信息

- 不入库：`src-tauri/binaries/`、`src-tauri/target/`、`src-tauri/gen/`、
  `dist/`、`node_modules/`、`output/`、本地日志。
- 打包资源（mihomo、geodata、Tor）由 `npm run fetch:sidecar` /
  `npm run fetch:tor` 获取，脚本校验哈希；不提交二进制。
- 真实主机 IP、凭据、私钥不得进入版本库；`npm run check:sensitive`
  与 CI 的 sensitive check 执行白名单校验（`scripts/sensitive-allowlist.txt`）。
- 测试源码只留在本机 `tests/`，CI 不运行测试门禁。

## 6. 检查命令

| 命令 | 覆盖内容 | CI |
|---|---|---|
| `npm run check:org` | 文档分层/命名、`src/` 顶层入口、views 互引 | 是 |
| `npm run check:size` | UI 组件 ≤500 行、单文件 ≤1500 行 | 是 |
| `npm run check:sensitive` | 真实 IP / 凭据不入库 | 是 |
| `npx tsc --noEmit` | 前端类型 | 是 |
| `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` | Rust 格式与静态检查 | 是 |
| `npm test`、`cargo test --features local-tests` | 本地测试（不随仓库分发） | 否 |

PR 上 CI 失败时，`ci-notice` job 会在 PR 里 @ 作者并列出失败 job 与具体失败步骤；
修复后同一条评论会更新为「CI 已通过」，不会重复刷屏。直接 push 到 master 的失败
由 GitHub Actions 对触发者的默认通知负责。

## 7. 变更本规范

修改规则时按以下顺序：

1. 先改本文件（规则事实源）；
2. 若可自动检查，改 `scripts/check-organization.mjs`（或新增检查脚本）并接入
   `.github/workflows/ci.yml`；
3. 在同一个 PR 中修正现有违规，否则 CI 无法变绿。
