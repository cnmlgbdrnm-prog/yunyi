# vpn-ctl P6 设计规格：CI 机器保证（GitHub Actions）

> 状态：已老板批准并实施（bc7841c）。战略文档见 `docs/product/strategy.md`（P9 已落地）。

## 1. 背景与目标

老板要求：把「个人纪律」（合并前全量测试、clippy、fmt 门禁）变成「机器保证」。

现状：仓库**无任何 CI**（`.github/workflows/` 不存在）。本地门禁全靠人工纪律：
- Rust：`cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（当前 288 测试）
- 前端：`npm test`（vitest run，107 测试）/ `npx tsc --noEmit -p tsconfig.json`

目标：push 与 PR 双触发，任一 job 红 → 不可合并。本地命令与 CI 命令**同一套**，杜绝「CI 与本地不一致」。

## 2. 关键约束与决策（老板已拍板）

| 决策 | 结论 |
|---|---|
| sidecar | 保留内置仅供测速（战略）；CI 必须能获取 mihomo.exe |
| sidecar 获取 | `npm run fetch:sidecar`（scripts/fetch-sidecar.mjs：下载→zip sha256→解压→exe sha256→落盘） |
| runner 平台 | **rust job 用 windows-latest**——fetch-sidecar 依赖 `powershell Expand-Archive` 解压（脚本现状），与老板本地 Windows 环境一致，不为此改脚本；frontend job 用 ubuntu-latest（纯 node，快） |
| 缓存 | Swatinem/rust-cache@v2（workspaces: src-tauri）——cargo 依赖编译是 CI 最大时间项 |
| Node | setup-node 22（本地 Node v24，22 是 LTS 稳定线；lockfile 兼容） |

## 3. Workflow 设计

文件：`.github/workflows/ci.yml`

```yaml
name: ci
on:
  push:
    branches: ['**']
  pull_request:

jobs:
  rust:
    runs-on: windows-latest
    defaults: { run: { working-directory: src-tauri } }
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: 'rustfmt,clippy' }
      - uses: Swatinem/rust-cache@v2
        with: { workspaces: 'src-tauri' }
      - uses: actions/setup-node@v4
        with: { node-version: 22 }
      # mihomo sidecar：tauri-build 校验 bundle.resources 存在性，cargo test 必须先行落盘
      - run: npm run fetch:sidecar
        working-directory: .
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test

  frontend:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with: { node-version: 22, cache: npm }
      - run: npm ci
      - run: npm test          # vitest run
      - run: npx tsc --noEmit -p tsconfig.json
```

要点：
- **命令一致性**：rust job 四步 = 本地门禁四命令；frontend 三步 = 本地同命令。CI 不发明新命令。
- **fetch:sidecar 幂等**：脚本本身幂等（已存在且哈希匹配 → 跳过）；CI 每次下载一次（~30MB），缓存策略留给 rust-cache 之外的首步。
- **pr 阻断语义**：master 分支保护启用「require checks: rust, frontend」（README 文档化，仓库设置由老板/维护者在 GitHub 上点一次）。
- **timeout**：rust job 首跑（冷缓存）预计 8-15 分钟（Windows runner + cargo 全量依赖），热缓存 4-6 分钟。不设激进 timeout，默认 360 分钟即可。

## 4. 不做什么（范围外）

- 不做 release 流水线（P8 自动更新时另建 release.yml，签名私钥走 GitHub Secrets）。
- 不做覆盖率上传/徽章（无需求）。
- 不跑 tauri build 产物（CI 只做门禁，不产物化——发布流水线是 P8 的事）。
- 不改 fetch-sidecar.mjs（windows runner 上原样可用）。

## 5. 测试与验收

1. **workflow 文件语法**：`actionlint` 不可用时的替代——人工核对 YAML 结构 + push 后 CI 实际跑绿。
2. **真实验收**：push 本分支 → CI 两 job 全绿（老板审 GitHub Actions 页面）；PR 场景同理。
3. **红灯验证**：故意提交一个 `cargo fmt` 不过的文件 → CI rust job 红（机器保证的证明）。红灯验证后 revert。
4. 本地与 CI 零差异：CI 中无本地不存在的命令。

## 6. 风险

- **Windows runner cargo 编译慢**：rust-cache 缓解；首跑慢可接受（一次性）。
- **GitHub 下载 mihomo zip 的网络**：GitHub Actions 在 GitHub 域内下载 GitHub Release 资产，速度与可用性最佳（脚本默认 URL 即 GitHub）。
- **npm ci 的 lockfile**：package-lock.json 已在仓库（工作树里有），npm ci 可直接用。
