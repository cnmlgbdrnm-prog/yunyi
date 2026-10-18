# vpn-ctl 部署发行版能力探测 + BBR 自动开启设计

日期:2026-09-11
状态:待评审

## 背景与目标

现状两个问题：

1. **部署面被发行版名字卡死**：`servers/deploy.rs:311` 的 `os_is_supported` 按 `/etc/os-release` 的 `ID`/`ID_LIKE` 做白名单，只放行 ubuntu/debian 家族，闸门在 `:707`。CentOS / RHEL / Fedora / Alma / Rocky / Arch / openSUSE 以及国内云厂商衍生版（Alibaba Cloud Linux / TencentOS / OpenCloudOS / Anolis）一律被拒。

   讽刺的是流水线本身几乎不依赖发行版：Xray 走官方 `install-release.sh`（该脚本自带 apt/yum/dnf 探测）；shadowsocks-rust 与 sing-box 都是 **GitHub Releases 二进制直落 `/usr/local/bin` + 工具自写 systemd unit**（`ss_systemd_unit()` / `singbox_systemd_unit()`），不依赖发布包。真正的发行版依赖只有三处：

   | # | 位置 | 依赖 |
   |---|---|---|
   | 1 | `deploy.rs:707` | `os_is_supported` 名字白名单 |
   | 2 | `deploy.rs:918` | `apt-get install -y openssl`（仅 sing-box 自签证书路径） |
   | 3 | `deploy.rs:974/994/1019` | `systemctl enable --now` + `is-active` |

2. **BBR 与部署流程脱节**：`server_bbr.rs` 的一键开启天然跨发行版（判据是内核有没有 `tcp_bbr`，不看发行版名），但它是服务器页面的独立行内按钮（模块文档：惰性触发，页面加载绝不自动 SSH），部署流水线完全不碰内核参数。用户装完服务器还得另外点一次。

目标：

1. 用**能力探测**取代名字白名单，让协议部署在任意 systemd 系发行版上可用；
2. 把 BBR 接入部署流水线，用户在部署向导里勾选即可顺手开启。

## 范围声明

- **只覆盖 systemd 系发行版**。无 systemd 的（Alpine/OpenRC、Devuan/sysvinit、Void/runit）明确报错，不假装支持。
- **不改 relay（中转链路）**。`relay/build.rs:845` 硬编码 `apt-get` 装 wireguard-tools 是另一条路径，本次不动。
- **不新增部署期调优项**。只接 BBR，不加 ulimit / rmem / wmem / tcp_fastopen / limits.conf。
- **不改 `server_bbr.rs`**。直接复用现有 `check_bbr` / `enable_bbr` 及其幂等脚本。
- 不动节点 / 订阅 / 测速 / 中转面。

## 一、能力探测取代名字白名单

### 删除

- `deploy.rs:311-316` `os_is_supported` 函数
- `deploy.rs:707-715` 的调用与错误分支
- `deploy_tests.rs:280-284` 对应用例，以及 `deploy_tests.rs:9` import 列表中的 `os_is_supported`

### 新增探测脚本（`deploy.rs` 常量）

```sh
. /etc/os-release 2>/dev/null; echo "OS=${ID:-unknown}${ID_LIKE:+ ${ID_LIKE}}"
if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then echo SYSTEMD=yes; else echo SYSTEMD=no; fi
```

两个判据都要，缺一不可：

- `/run/systemd/system` 是 systemd 作为 PID 1 时创建的目录。容器 / WSL 里 `systemctl` 二进制存在但 systemd 未运行；只看命令存在会走到「unit 文件写完 → `systemctl enable` 静默不生效 → 端口验证失败」的半截状态，失败点离原因太远。
- `command -v systemctl` 兜住「systemd 在跑但 PATH 里没有 systemctl」的小概率情形。

`. ` 用 POSIX 点号而非 `source`（busybox ash 不认 `source`）；`${ID:-unknown}` 兜底缺失字段。

一次 SSH 往返同时拿到系统标识（用于报错文案）与 systemd 判定，进现有探测阶段（`cat /etc/os-release` 那条命令被本脚本取代）。

### 解析函数

```rust
pub fn parse_capability(stdout: &str) -> (String, bool)   // (OS 描述, 是否 systemd)
```

- `OS=` 前缀行 → 描述串，trim；缺失 → `"未知"`
- 存在 trim 后等于 `SYSTEMD=yes` 的行 → `true`

### 失败语义

`SYSTEMD=no` →

```
该系统未运行 systemd（ID=alpine like=），暂不支持自动部署
```

信息可操作：用户看得出缺的是哪个能力，而不是笼统的「不支持的系统」。

## 二、openssl 安装改为包管理器探测

`deploy.rs:918` cert_cmd 中的 `apt-get install -y openssl` 换成自包含探测脚本，分支顺序与写法对齐 `server_speed/iperf.rs:125-129` 的既有实现（同仓已有先例，非新发明）：

| 顺序 | 包管理器 | 命令 |
|---|---|---|
| 1 | apt-get | `apt-get update -qq && apt-get install -y openssl` |
| 2 | dnf | `dnf install -y openssl` |
| 3 | yum | `yum install -y openssl` |
| 4 | pacman | `pacman -Sy --noconfirm openssl` |
| 5 | zypper | `zypper -n install openssl` |
| 6 | apk | `apk add openssl` |
| 兜底 | — | `echo NO_OPENSSL; exit 1` |

调用点从 `run_cmd_ok` 改为 `run_cmd`，以便识别 `NO_OPENSSL` token：

- stdout 含 `NO_OPENSSL` → `Err("该系统找不到 openssl，也没有可识别的包管理器（apt-get/dnf/yum/pacman/zypper/apk），无法生成 sing-box 自签证书")`
- 退出码非 0 → 沿用现有 `生成自签证书失败(exit=N): {stderr}` 语义

### 影响面

`if ! command -v openssl` 前置守卫意味着：RHEL 系 / Arch 上 openssl 是 curl 的依赖、默认存在，安装分支基本不会触发。该分支只是安全网。证书生成本身仍是幂等（`[ ! -f /etc/sing-box/self.crt ]` 守卫），不变。

## 三、BBR 接入部署流水线

### 插入点

`deploy.rs:1051`（逐协议端口验证循环结束）与 `:1054`（写回）之间。

**必须在端口验证之后**：协议服务确认起来了才动内核参数，BBR 出问题不可能被误判为部署失败。

### 编排

```rust
let mut bbr_note: Option<String> = None;
if req.enable_bbr {
    emit_progress("优化", "检测 BBR", 88);
    match server_bbr::check_bbr(&mut sess).await {
        Ok(algo) if algo == "bbr" => bbr_note = Some("BBR 已启用".to_string()),
        Ok(algo) => {
            emit_progress("优化", &format!("当前 {algo}，开启 BBR"), 90);
            bbr_note = Some(match server_bbr::enable_bbr(&mut sess).await {
                Ok(m) => m,                                   // "已开启 bbr"
                Err(e) => format!("开启失败：{e}"),
            });
        }
        Err(e) => bbr_note = Some(format!("检测失败：{e}")),
    }
}
```

已是 `bbr` 时**不下发开启脚本**（`server_bbr.rs` 的脚本虽幂等，但没必要多跑一次 sysctl 写）。

三条成功/失败路径产出**四种不同文案**，实现时照抄，不要合并：

| 情形 | `bbr` 字段值 |
|---|---|
| 检测即 `bbr`（未改动服务器） | `BBR 已启用` |
| 检测非 `bbr` → 开启成功 | `已开启 bbr`（`enable_bbr` 的返回值，勿改写） |
| 开启失败 | `开启失败：{原因}` |
| 检测失败 | `检测失败：{原因}` |

前两条刻意不同：一个表示「本来就是」，一个表示「本次改动生效了」，用户能区分。

### 失败语义

**BBR 的任何失败都不上抛**。协议已部署成功，BBR 属优化项；加 `?` 会让一次成功的部署在最后一步变成 failed。失败原因收集成字符串，随摘要与完成文案上屏。

### 落库

deploys 摘要 JSON 增加 `"bbr"` 字段（`Some` 才写）：

```json
{"protocols":["vless"],"ports":{"vless":443},"bbr":"已开启 bbr"}
```

`parse_deployed_protocols`（`deploy.rs:64`）只读 `protocols` 字段、容忍额外字段（已核对实现），不破坏既有解析。

### 进度与完成文案

- 新步骤「优化」，进度 88 → 90，复用现有 `emit_progress` 通道；`DeployWizard.tsx:254-263` 的进度区直接渲染 `step`/`message`，**前端零改动即可显示**
- 100% 完成文案带 BBR 结果：有 note 时 `部署完成（已开启 bbr）`，无 note 时保持 `部署完成`

现有断言 `deploy_tests.rs:582-594` 只查步骤集合包含 探测/安装/配置/启动/写回 五步、首为「探测」末为「写回」；新增「优化」步骤位于写回之前，不破坏该断言（已核对）。

## 四、接口与前端

| 文件 | 改动 |
|---|---|
| `server_bbr.rs` | **不动** |
| `jobs/mod.rs:29` | `DeployRequest` 加 `enable_bbr: bool` |
| `domain.rs:203` | `DeployInput` 加 `enable_bbr: Option<bool>` |
| `commands.rs:410` `start_deploy` | 透传 `enable_bbr: input.enable_bbr.unwrap_or(false)` |
| `commands.rs:1086` `start_deploy_cmd` | 加 `enable_bbr: Option<bool>` 参数（与 `reality_dest` 同款可选写法） |
| `commands.rs:471` `install_supplement` | 其 `DeployInput` 填 `enable_bbr: None` |
| `api/types.ts:132` | `DeployInput` 加 `enableBbr?: boolean` |
| `api/index.ts:218` | **不动**（`invoke('start_deploy_cmd', { ...input })` 自动透传） |
| `DeployWizard.tsx` | 协议网格下方加复选框 |

### 已确认的判断点

1. **复选框默认勾选，不持久化到 localStorage**（协议选择是持久化的，本项不跟随）。理由：BBR 幂等，已开启的机器再勾也只是检测后跳过。
2. **补装（`install_supplement`）不触发 BBR**：其 `DeployInput` 传 `None`。补装是「再加一个协议」，首装时已问过一次。
3. **BBR 失败不算部署失败**（见第三节失败语义）。

### 复选框

- label「同时开启 BBR」，`aria-label` 同名
- 默认 `true`
- `running` 时 `disabled`（与协议复选框一致）
- 提交时始终传布尔值 `enableBbr`（`{...input}` 已透传，`api/index.ts` 无需改）

## 五、测试计划

### 后端

- `deploy_tests.rs`
  - 删 `os_is_supported` 用例（`:280-284`）与 import
  - `parse_capability` 三例：`SYSTEMD=yes` / `SYSTEMD=no` / 两个标记行都缺失
  - cert 命令：断言六个包管理器分支字符串齐备 + `NO_OPENSSL` 兜底存在
  - BBR 编排四例（FakeSshServer 脚本注入）：
    1. `check_bbr` 回 `bbr` → **不下发**开启脚本（`recorded()` 无 `BBR_ENABLE_SCRIPT`），摘要记「已启用」
    2. 回 `cubic` → 下发开启脚本、回读 `bbr` → 摘要记「已开启 bbr」
    3. `enable_bbr` 失败（`NO_BBR_MODULE`）→ **部署仍成功**（`Ok(())`），摘要 `bbr` 字段记失败原因
    4. `enable_bbr: false` → 无「优化」步骤事件，摘要无 `bbr` 字段
- `jobs/tests.rs:109` `req()` 补 `enable_bbr: false`
- `commands_tests.rs` 各处 `DeployInput` 字面量补 `enable_bbr` 字段（`:235/262/270/280` 附近）
- `ipc_tests.rs` `start_deploy_cmd` 加 `enableBbr` camelCase 用例（照 `:792` BBR 那组写法）

### 前端

- `DeployWizard.test.tsx`：默认勾选；勾选时 payload `enableBbr: true`；取消勾选后 payload `enableBbr: false`

## 六、不做的事

- Alpine/OpenRC 及其他非 systemd init（探测到就明确报错，不实现）
- relay 链路 `relay/build.rs:845` 的 apt 硬编码
- BBR 之外的部署期内核调优（ulimit / rmem / wmem / tcp_fastopen / limits.conf）
- BBR 结果的服务器页持久化展示（服务器页仍靠行内按钮自行查询）
- 补装流程的 BBR 触发

## 风险

- **`/run/systemd/system` 判据**：极少数发行版把 systemd 装在非标准位置会误判。风险可忽略，且失败表现是明确报错而非静默错误。
- **`dnf install -y openssl` 在离线机器上失败**：归入证书步骤的可读错误，不影响其他协议（sing-box 之外的协议不需要 openssl）。
- **BBR 失败被用户忽略**：写进 100% 完成文案，不点开摘要也能看到。
