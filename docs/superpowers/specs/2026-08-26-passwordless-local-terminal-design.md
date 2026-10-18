# 一键免密覆盖本机终端（双钥并行）设计

> 日期：2026-08-26。现状：`setup_passwordless`（`src-tauri/src/servers/mod.rs:371`）生成的密钥只供 App 使用——私钥 DPAPI 密封存 App 数据库，与 PowerShell OpenSSH 客户端使用的 `~/.ssh` 无关，导致服务器 `authorized_keys` 中只有 App 的公钥，终端 `ssh user@host` 仍要密码。本规格将该缺口补齐。

## 1. 范围与验收

- 点击「一键免密」后，除 App 密钥外，**本机终端 `ssh user@host` 也免密**。
- App 现状不变：每台服务器独立生成一把密钥、私钥仅存 App 数据库（密封），服务器间互不牵连。
- 本机已有 `~/.ssh` 标准私钥（`id_ed25519`/`id_rsa`/`id_ecdsa`）→ 其公钥一并登记到服务器。
- 本机一个密钥都没有 → 自动生成 ed25519 写入 `~/.ssh/id_ed25519(+.pub)`（注释 `vpn-ctl-local`，权限对齐 `ssh-keygen` 行为），公钥下发。
- **验收**：本机 `ssh -o BatchMode=yes root@<host> echo ok` 返回 ok；重复点击幂等；本机准备失败时整个免密中止且数据库无变化。

## 2. 已确认决策（老板 2026-08-26）

| 项 | 决策 |
|---|---|
| 本机钥匙策略 | **双钥并行**：App 保持每服务器独立钥（隔离性不变），本机密钥登记为额外行 |
| 本机无密钥 | **自动生成**（russh 现成依赖，不依赖外部 ssh-keygen） |
| 失败处理 | 本机准备（扫描/生成/写文件/icacls）任一失败 → **整个免密失败并中止**，不落库 |
| 登记哪些本机密钥 | `~/.ssh` 下存在的标准私钥（id_ed25519/id_rsa/id_ecdsa）；只有私钥无 .pub 时亦从私钥解析公钥 |
| 生成钥注释 | `vpn-ctl-local`（与 App 钥 `vpn-ctl-admin` 同风格，可溯源） |

## 3. 架构

```
src-tauri/src/servers/mod.rs
  setup_passwordless()                # 编排：现有 App 钥链路不变，其后追加「本机免密准备」
  prep_local_keypair(ssh_dir)         # 新：扫描/生成钥，返回待登记公钥行列表（ssh_dir 可注入，测试传临时目录）
  -- 每个待登记公钥行：grep -qF 幂等追加 ~/.ssh/authorized_keys（同现有写法 mod.rs:404）
```

步骤（在原函数内、App 钥验证通过后、凭据切换之前；同一 SSH 会话）：

1. 扫描 `ssh_dir`（生产传 `~/.ssh`，即 `USERPROFILE\.ssh`）：标准私钥存在 → 读 `.pub` 或由 russh `decode_secret_key` 解出公钥行。
2. 一个都没有 → russh 生成 ed25519（`PrivateKey::random`），写 `id_ed25519`（私钥）与 `id_ed25519.pub`（含注释 `vpn-ctl-local`；`~/.ssh` 不存在则先创建）。**私钥文件与新建的 `~/.ssh` 目录**都用 `std::process::Command` 调 `icacls` 设置：关继承 + 当前用户/SYSTEM/Administrators 授权（对齐 `ssh-keygen` 权限——Win OpenSSH 对私钥文件和目录都会检查「权限过宽」）。
3. 服务器侧继续用 `run_ok` 对每个公钥行执行幂等追加；局部失败被 `run_ok` 覆盖。
4. 本机准备任何一步失败 → 整个调用返回 `ServersError`（沿用现有错误体系，UI 已能显示），**不触碰数据库事务**。

数据流变化：仅 `setup_passwordless` 内部新增步骤；DB 迁移、凭据表、前端 API 均不动。

## 4. 测试

- 沿用 `servers/tests.rs` FakeSshServer + 可注入目录：
  - 有现存本地密钥 → 下发命令序列包含本机公钥行；
  - 无任何密钥 → 临时目录生成两文件（私钥可被 russh 解析、公钥行含 `vpn-ctl-local`）且下发；
  - 幂等：重复 `setup_passwordless` 下发序列不重复追加（grep 守卫）；
  - 现有 `setup_passwordless_switches_to_key_and_deletes_password` 扩展 FakeSshServer 脚本适配新增命令，断言语义不变。

## 5. 文档

README「一键免密」描述补充：除 App 密钥外，本机 `~/.ssh` 已有密钥一并登记，无密钥时自动生成（终端 `ssh` 随即免密）。
