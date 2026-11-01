# 部署加固：fail2ban（SSH 爆破防护）

部署管道在「放行」之后、「写回」之前执行「加固」步骤。每次部署都会执行，幂等；旧服务器在下一次部署时自动获得。

## 规范（2026-10-29 定）

配置写在 `/etc/fail2ban/jail.d/vpn-ctl.local`，只定义 `sshd` jail，不写 `[DEFAULT]` 段，避免影响机器上已有的其它 jail。

| 项 | 值 | 理由 |
| --- | --- | --- |
| jail | 仅 `sshd` | 部署不知道其它服务的日志格式，乱配 jail 只会误封 |
| `backend` | `systemd` | Ubuntu 22.04+/Debian 12 默认没有 `/var/log/auth.log`；sshd 过滤器自带 `_COMM=sshd` journalmatch，Debian 的 `ssh.service` 单元名也能命中 |
| `findtime` | `1m` | 统计窗口 |
| `maxretry` | `5` | 1 分钟内 5 次失败 |
| `bantime` | `-1`（永久封禁） | 用户明确选择的策略：不给爆破方冷却重试的机会；记录持久化在 fail2ban 库/规则里，服务重启、机器重启都不解除 |
| `port` | 应用连接用的 SSH 端口 | 只封管理端口，不封业务端口——代理端口被滥用的形态与 SSH 爆破不同，没有通用过滤器 |
| `ignoreip` | `127.0.0.1/8 ::1` | 管理 IP 是动态的，硬编码白名单等于没有保护；密钥登录失败通常不触发 sshd 过滤器 |
| `banaction` | **不覆盖**，沿用系统 fail2ban 的 DEFAULT | 各发行版/版本后端不同（Ubuntu 26.04 的 fail2ban 1.1 默认 `nftables`，Debian 12 是 `iptables-multiport`，管理员也可能已配置 `ufw`）。中途切换动作会让 fail2ban 用新动作恢复旧封禁、旧动作的规则没人清理 |

安装：Debian/Ubuntu 用 `apt-get install fail2ban python3-systemd`（`python3-systemd` 是 systemd 后端依赖，apt 只列为 Recommends）；RHEL 系先启用 EPEL。已安装且 `import systemd.journal` 可用时跳过安装。

验证：`systemctl restart fail2ban` 返回 0 不代表 jail 加载成功，必须 `fail2ban-client status sshd` 成功才算通过。失败最多重试 10 秒。

## 失败语义

与 BBR 相同：协议已经验证通过，加固失败只记入部署摘要的 `fail2ban` 字段并显示在完成文案里，**不把部署判成 failed**。摘要示例：

```json
{ "protocols": ["vless"], "bbr": "BBR 已启用", "fail2ban": "fail2ban 已启用（sshd 端口 22：1 分钟 5 次失败永久封禁）" }
```

## 封禁名单跨机同步（增量下发）

- 分享状态轮询每 2 分钟连一遍所有服务器，顺带在每台执行
  `fail2ban-client get sshd banip`，把封禁 IP 并入本地 `banned_ips` 表（迁移 v20）。
  没装 fail2ban 的服务器输出为空，属于正常状态，不影响分享快照。轮询只在应用
  运行时进行。
- **只发增量**：`server_ban_sync` 表（迁移 v21）记录每台服务器已处理到的
  `banned_ips.rowid` 水位。每轮只取水位之后、目标机当前还没有的 IP 下发（每台
  每轮最多 1000 条），已经有的不重发。A 上新封的 IP 会在一轮内到 B/C；
  下发后回读 `get sshd banip` 核验，未全部生效则不推进水位、下一轮重试。
- **首次见到服务器时基线化**：以当前最大 rowid 为水位，不在轮询里补推历史名单；
  历史名单只在新机器部署时全量上传（见下）。
- **手工 unban 不会被下一轮重新封回**：水位已越过该 IP。但重新部署这台服务器
  时，部署会按「本地有、服务器没有」的差集再下发一次；要永久排除需同时从本地
  `banned_ips` 移除。
- **跳过应用自身的 SSH 来源 IP**：每台服务器用 sshd 注入的 `SSH_CLIENT` 取当前
  会话地址；如果它在名单里（例如以前输错过密钥），不下发，避免把管理通道封死。
- 未安装 fail2ban 的服务器直接跳过（部署时会全量补上），不反复重试；jail 未
  就绪则保留水位、下一轮再试。
- **首次部署 / 重部署**：「加固」步骤先读取该机现有封禁，算出与本地名单的差集：
  - 差集 ≤ 1000：同步批量下发（每批 200 个 IP 一次调用，1000 条约 25 秒）；
  - 差集 > 1000：分块上传名单后 `setsid` 分离启动后台 worker，每批 200 条、
    批间 `sleep 2` 慢慢封（1 万条约 6 分钟），部署立即继续；进度写在
    `/tmp/vpn-ctl-preban.log`，完成后自动删除名单与 pid 文件；
  - 重部署只补差集，不重发整份名单；
  - 下发成功后把该服务器水位推进到当前最大 rowid，轮询不会重复下发。
- 本地名单在应用数据库 `banned_ips` 表里，可用任意 SQLite 工具查看；水位在
  `server_ban_sync` 表：

```sql
SELECT ip, first_seen_at, last_seen_at, last_server_id
FROM banned_ips ORDER BY last_seen_at DESC;

SELECT server_id, last_rowid, updated_at FROM server_ban_sync;
```

## 查看与解封

永久封禁不会自己过期，必须人工解除：

```sh
fail2ban-client status sshd          # 当前封禁列表
fail2ban-client get sshd banip       # 只列 IP
fail2ban-client set sshd unbanip 1.2.3.4
fail2ban-client unban --all          # 清空所有 jail
```

被自己的 IP 误封时，从服务商控制台登录后执行上面的解封命令；`ignoreip` 只放行回环，没有预置管理 IP。解封后该 IP 的封禁记录从 fail2ban 库中移除，重启不会复用旧封禁，轮询也不会重新下发（水位已越过它）；但下次重新部署这台服务器时会被差集再次补上。

## 真机验证（2026-10-29）

- ashura（Ubuntu 26.04 / fail2ban 1.1.0）：写入 drop-in 并重启后
  `findtime=60`、`maxretry=5`、`bantime=-1` 生效，原有 21 条封禁在重启后保留。
- 预封禁用 TEST-NET 地址（192.0.2.1、198.51.100.7）实测：`set sshd banip` 后
  出现在 `get sshd banip`，fail2ban 重启后仍在（永久语义），`unbanip` 后清除。
- 批量下发（2026-10-29，ashura）：一次调用封 1000 个 benchmark 地址
  （198.18.0.1–198.18.3.250）24.8 秒全部生效，解封同样一次调用 28.9 秒
  （`fail2ban-client` 返回 1000）；原有 21 条真实封禁全程不受影响。逐条调用
  实测 ~257ms/条，1000 条要 4 分钟以上——据此把预封禁改为每批 200 个 IP 一次
  调用。
- 本地同步：应用轮询一轮后 `banned_ips` 表收到 ashura 21 条 + VM-irO0oMfP 24 条，
  共 45 条（并集，按最近上报记录来源服务器）。
- 后台预封禁（2026-10-29，ashura）：用 5 个 TEST-NET 地址实测 `setsid` 分离启动，
  SSH 会话关闭后 worker 继续执行；日志依次出现 `start / batch / done`，名单文件与
  pid 文件自动删除，5 条封禁全部生效后立即解封，原有 21 条真实封禁未受影响。

## 已知边界

- 跨机增量同步依赖桌面应用运行：应用关闭期间各服务器仍各自封禁，重新打开后会把
  游标之后的新封禁补齐；功能首次上线时以当时名单为基线，不补推历史名单（历史
  名单只在新机器部署时全量上传）。
- 只保护 SSH；代理入站（Xray/sing-box/SS）的滥用没有对应过滤器。
- 动态管理 IP 未加入白名单，1 分钟内输错 5 次密码/密钥配置错误会**永久**失去 SSH 访问，只能从服务商控制台解封。
- RHEL 系如果以 firewalld 管理规则，`iptables-multiport` 的封禁可能被 firewalld reload 清掉；本工具的部署目标是 Debian/Ubuntu 为主的机器。
