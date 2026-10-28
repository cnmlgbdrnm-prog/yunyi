//! 数据库迁移：按版本顺序执行，单个迁移在事务内应用、失败整体回滚；
//! PRAGMA user_version 记录当前版本。

use rusqlite::Connection;

/// 全部迁移，索引即版本号（v1 = 初始 8 表）。
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE servers (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  host TEXT NOT NULL,
  port INTEGER NOT NULL DEFAULT 22,
  user TEXT NOT NULL DEFAULT 'root',
  auth_kind TEXT NOT NULL DEFAULT 'password' CHECK (auth_kind IN ('password','key')),
  auth_ref TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('relay','landing')),
  system TEXT NOT NULL DEFAULT '',
  notes TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL
);
CREATE TABLE credentials (
  id TEXT PRIMARY KEY,
  encrypted_blob TEXT NOT NULL
);
CREATE TABLE subscriptions (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  url TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  last_sync_at TEXT,
  sync_interval INTEGER NOT NULL DEFAULT 86400,
  last_sync_error TEXT,
  filter_json TEXT NOT NULL DEFAULT '{"include":[],"exclude":[]}'
);
CREATE TABLE nodes (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('import','self-built','manual')),
  protocol TEXT NOT NULL,
  addr TEXT NOT NULL,
  port INTEGER NOT NULL,
  params_json TEXT NOT NULL DEFAULT '{}',
  group_name TEXT NOT NULL DEFAULT '默认分组',
  sub_id TEXT REFERENCES subscriptions(id) ON DELETE SET NULL,
  relay_rule_id INTEGER,
  enabled INTEGER NOT NULL DEFAULT 1,
  latency INTEGER,
  bandwidth INTEGER,
  last_checked_at TEXT
);
CREATE TABLE relay_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  relay_server_id INTEGER NOT NULL REFERENCES servers(id),
  landing_server_id INTEGER NOT NULL REFERENCES servers(id),
  mode TEXT NOT NULL CHECK (mode IN ('wg','gost-tls')),
  listen_port INTEGER NOT NULL,
  landing_port INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'not-built'
);
CREATE TABLE deploys (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id),
  kind TEXT NOT NULL,
  config_json TEXT NOT NULL,
  deployed_at TEXT NOT NULL,
  status TEXT NOT NULL
);
CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE jobs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,
  progress INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  created_at TEXT NOT NULL
);
"#,
    r#"
-- relay_rules 重建：P1 版无写入路径，空表确认后重建；mode 收敛为 wg
DROP TABLE relay_rules;
CREATE TABLE relay_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  relay_server_id INTEGER NOT NULL REFERENCES servers(id),
  landing_server_id INTEGER NOT NULL REFERENCES servers(id),
  client_mode TEXT NOT NULL CHECK (client_mode IN ('forward','peer')),
  wg_port INTEGER NOT NULL DEFAULT 51820,
  forward_port INTEGER,
  landing_port INTEGER NOT NULL,
  subnet TEXT NOT NULL,
  relay_wg_ip TEXT NOT NULL,
  landing_wg_ip TEXT NOT NULL,
  converged INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'not-built'
);
CREATE TABLE device_peers (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  rule_id INTEGER NOT NULL REFERENCES relay_rules(id),
  name TEXT NOT NULL,
  public_key TEXT NOT NULL,
  private_key TEXT NOT NULL,
  psk TEXT NOT NULL,
  wg_ip TEXT NOT NULL,
  created_at TEXT NOT NULL
);
"#,
    r#"
-- v3：WG 公钥入规则行——状态查询/拆链按公钥定位 peer，不依赖接口名（真机验收发现
-- 手动建的链路接口名可为 wg1 等非 wg0）。公钥为公开材料，非敏感。
ALTER TABLE relay_rules ADD COLUMN relay_pubkey TEXT;
ALTER TABLE relay_rules ADD COLUMN landing_pubkey TEXT;
"#,
    r#"
-- v4：节点测速失败原因（成功置 NULL；latency/bandwidth 已有，P4 填充）
ALTER TABLE nodes ADD COLUMN last_check_error TEXT;
"#,
    r#"
-- v5：服务器测速（P5）——servers 4 列语义同 nodes（latency ms/bandwidth bytes/s/
-- last_checked_at RFC3339/last_check_error 失败原因）；server_throughput 一次对测一行
ALTER TABLE servers ADD COLUMN latency INTEGER;
ALTER TABLE servers ADD COLUMN bandwidth INTEGER;
ALTER TABLE servers ADD COLUMN last_checked_at TEXT;
ALTER TABLE servers ADD COLUMN last_check_error TEXT;
CREATE TABLE server_throughput (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  from_server_id INTEGER NOT NULL REFERENCES servers(id),
  to_server_id INTEGER NOT NULL REFERENCES servers(id),
  bits_per_second INTEGER,
  checked_at TEXT NOT NULL,
  error TEXT
);
"#,
    r#"
-- v6：对测失败行完整客户端输出落库（P5.7）——真机连续失败时 error 仅 120 字
-- 预览无法定位 JSON 结构差异（老版 iperf3 流级统计），完整 stdout/stderr 供诊断
ALTER TABLE server_throughput ADD COLUMN full_output TEXT;
"#,
    r#"
-- v7：通道管理——每台服务器可登记端口通道（防火墙放行/封锁开关），
-- 本地 UI 直接管理开关；service_unit 列在 v8 移除（开关语义改为防火墙端口控制）
CREATE TABLE channels (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id),
  name TEXT NOT NULL,
  port INTEGER NOT NULL,
  protocol TEXT NOT NULL,
  service_unit TEXT NOT NULL,
  created_at TEXT NOT NULL
);
"#,
    r#"
-- v8：端口开关语义——开关改为服务器防火墙放行/封锁（iptables INPUT 链），
-- 不再依赖 systemd 服务单元名；service_unit 列移除
ALTER TABLE channels DROP COLUMN service_unit;
"#,
    r#"
-- v9：中转节点延迟对比——挂了中转规则的节点额外测一次「直连形态」延迟，与经中转值
-- 同口径（都是 mihomo URL 测试），可直接相减得出中转收益。可空：NULL = 未测过对比。
ALTER TABLE nodes ADD COLUMN direct_latency INTEGER;
"#,
    r#"
-- v10：分跳延迟（裸 TCP RTT，各 1 个往返）：本机→中转机、本机→落地机、中转机→落地机
-- （经 WireGuard 隧道，故量隧道内地址而非落地机公网 IP）。仅挂中转的节点会测，
-- NULL = 未测/失败。与 latency（经代理的完整 HTTP 往返、每段 ≥2 个往返）口径不同，
-- 三者相加 ≠ latency，只作「哪一跳烂」的定位指标。
ALTER TABLE nodes ADD COLUMN hop_relay_ms INTEGER;
ALTER TABLE nodes ADD COLUMN hop_landing_ms INTEGER;
ALTER TABLE nodes ADD COLUMN hop_tunnel_ms INTEGER;
"#,
    r#"
-- v11：上游出口（静态住宅 IP 代理作为中转机的出站）。一条记录 = 中转机上专用 Xray
-- 实例里的一个 inbound + 一个 outbound + 一条 routing 配对；多出口靠 inboundTag 配对共存。
-- password 为 seal 密文；material_json 首建时生成后固定（重建复用，否则客户端节点失效）。
CREATE TABLE upstreams (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id),
  name TEXT NOT NULL,
  proto TEXT NOT NULL CHECK (proto IN ('socks5','http')),
  host TEXT NOT NULL,
  port INTEGER NOT NULL,
  username TEXT NOT NULL DEFAULT '',
  password TEXT NOT NULL,
  listen_port INTEGER NOT NULL,
  reality_dest TEXT NOT NULL,
  material_json TEXT NOT NULL,
  node_id TEXT,
  egress_ip TEXT,
  last_check_error TEXT,
  created_at TEXT NOT NULL
);
"#,
    r#"
-- v12：链路转发模式——'single' 为既有语义（一个落地端口一条链路，可重映射入口端口）；
-- 'same_port' 表示这条链路把该落地机的**全部自建节点端口按原端口号**转发出去
-- （一条链路承载整机多协议：112 上 vless/vmess/trojan 三个端口今天就卡在这里）。
-- 存量行默认 single，行为逐字节不变。
ALTER TABLE relay_rules ADD COLUMN forward_mode TEXT NOT NULL DEFAULT 'single';
"#,
    r#"
-- v13：隐私保护（Tor Onion 入口）。一条记录 = **某条既有节点线路的 Tor 入口**。
--
-- 形状是「开关」不是「新节点」：只为那条线开一个 HiddenService，转发到
-- **它自己已经监听的端口**（`port`），Xray 配置一个字节都不动。于是：
--   · 不新建入站、不新建凭据、不新建出站；
--   · 出口自动就是那条线原本的出口（有 out-up-* 就是住宅 IP，没有就是 freedom）；
--   · 客户端只把 `server` 换成 onion + 挂 `dialer-proxy`，协议栈原样。
--
-- 为什么单开一张表而不是加列到 nodes：拆除必须「只删自己这一份 onion 身份，
-- 不碰别的线路」，而 server_id/node_id 是关系不是节点参数（节点表用 protocol+addr
-- 定位会误删兄弟行，见 servers/upstream.rs 的既有教训）。
--
-- tor_dir 是这条独享的 HiddenServiceDir，删除时按它精确清理；
-- onion_host 由远端 hostname 文件回读，**私钥永不入库**。
CREATE TABLE privacies (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id),
  node_id TEXT NOT NULL,
  name TEXT NOT NULL,
  port INTEGER NOT NULL,
  tor_dir TEXT NOT NULL,
  onion_host TEXT NOT NULL DEFAULT '',
  last_check_error TEXT,
  created_at TEXT NOT NULL
);
"#,
    r#"
-- v14：隐私入口「开在哪台机器、转发到哪里」。
--
-- v13 把这两件事写死了：server_id 是节点自己那台机器、转发目标恒为 127.0.0.1:port。
-- 那只在**直连**线路上成立。挂了中转之后入口不在落地机：
--   · forward 模式：客户端连的是中转机的 `forward_port`，由中转机的
--     `iptables -t nat -A PREROUTING -i eth0 --dport {entry} -j DNAT \
--      --to-destination {landing_wg_ip}:{landing_port}` 送进隧道；
--   · peer 模式：客户端自己就是隧道里的 peer，连 `{landing_wg_ip}:{landing_port}`。
-- 两种情形下客户端连的都不是落地机的公网端口，而且**收敛会把落地机的入站监听地址
-- 从 0.0.0.0 改成 landing_wg_ip**（relay/build.rs 的 rewrite_listen），所以那台机器上
-- 的 `127.0.0.1:{port}` 压根没有在听——隐藏服务必须转发到隧道目标。
--
-- 于是：server_id 的含义收窄为「**隐藏服务部署在哪台机器上**」（直连线＝落地机，
-- forward 中转线＝中转机），port 保持「onion 侧监听的端口」（= 客户端配置里的 port，
-- 即 effective_node 算出来的入口端口）；新增的两个列记转发目标。
--
-- 存量行都是 v13 的直连线：转发目标就是它们自己的 127.0.0.1:port，照此回填。
ALTER TABLE privacies ADD COLUMN fwd_host TEXT NOT NULL DEFAULT '127.0.0.1';
ALTER TABLE privacies ADD COLUMN fwd_port INTEGER NOT NULL DEFAULT 0;
UPDATE privacies SET fwd_host = '127.0.0.1', fwd_port = port;
"#,
    r#"
-- v15：服务器双栈地址。host 保持 IPv4/域名；host_v6 为可选的 IPv6 字面量（空串=没有）；
-- addr_family 决定 SSH 管理、服务器测速与自建节点当前使用的地址族（v4/v6）。
-- 切换由 set_server_addr_family_cmd 在同一事务内同时改服务器与节点 addr，避免
-- 「服务器切了、节点还指旧地址」的错位。
ALTER TABLE servers ADD COLUMN host_v6 TEXT NOT NULL DEFAULT '';
ALTER TABLE servers ADD COLUMN addr_family TEXT NOT NULL DEFAULT 'v4';
"#,
    r#"
-- v16：服务器出口地址族。与 addr_family（本机→VPS 的入口/管理地址）完全独立：
-- egress_family 决定 VPS 再访问目标域名时用 IPv4 还是 IPv6 出站（v4/v6）。
-- 该值在部署时写进三类服务端配置，切换后需要重部署才生效。
ALTER TABLE servers ADD COLUMN egress_family TEXT NOT NULL DEFAULT 'v4';
"#,
    r#"
-- v17：服务器健康监控。
-- 样本按轮写入；ts 是客户端 UTC RFC3339（毫秒精度，固定宽度，便于字典序比较）。
CREATE TABLE server_metric_samples (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  ts TEXT NOT NULL,
  reachable INTEGER NOT NULL,
  connect_ms INTEGER,
  uptime_secs INTEGER,
  load1 REAL,
  load5 REAL,
  load15 REAL,
  cpu_pct REAL,
  cpu_cores INTEGER,
  mem_used_kb INTEGER,
  mem_total_kb INTEGER,
  disk_used_kb INTEGER,
  disk_total_kb INTEGER,
  bbr_algo TEXT,
  services_json TEXT,
  wg_json TEXT,
  error TEXT
);
CREATE INDEX idx_server_metric_samples_server_ts
  ON server_metric_samples(server_id, ts);
CREATE INDEX idx_server_metric_samples_ts ON server_metric_samples(ts);

CREATE TABLE server_alerts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  severity TEXT NOT NULL CHECK (severity IN ('warning','critical')),
  opened_at TEXT NOT NULL,
  resolved_at TEXT,
  acked_at TEXT,
  last_value TEXT,
  last_at TEXT NOT NULL,
  message TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_server_alerts_active
  ON server_alerts(server_id, kind) WHERE resolved_at IS NULL;
CREATE INDEX idx_server_alerts_opened_at ON server_alerts(opened_at);
"#,
    r#"
-- v18：分享域名。每台服务器一个，解析到该机；新分享用它签证书并生成 https 链接。
-- 空串 = 未配置，新建分享会被拒绝（绝不退回 HTTP）。
ALTER TABLE servers ADD COLUMN share_domain TEXT NOT NULL DEFAULT '';
"#,
    r#"
-- v19：上游出口复用主入口。此前每条出口都在专用 xray-upstream 实例里再开一个
-- VLESS+REALITY 端口（listen_port）；用户已有协议时既多开端口又不能切换协议。
-- entry_proto 非空 = 复用主配置里同协议的入站（entry_port 是它的端口，部署回写时
-- 校正；0 = 该协议尚未部署，下一次部署会补上）。entry_node_id 指向复用入口的主节点，
-- 仅用于展示与失效检测；真正渲染按 entry_proto 匹配（主配置每协议一个入站）。
-- 存量行 entry_proto 为空 = 保持旧专用实例语义，逐字节不变。
ALTER TABLE upstreams ADD COLUMN entry_node_id TEXT;
ALTER TABLE upstreams ADD COLUMN entry_proto TEXT NOT NULL DEFAULT '';
ALTER TABLE upstreams ADD COLUMN entry_port INTEGER NOT NULL DEFAULT 0;
"#,
];

/// 当前 schema 版本（= 已应用迁移条数）。测试断言用，避免把版本号写死在断言里——
/// 每加一条迁移都要去改散落各处的魔法数字。
#[cfg(all(test, feature = "local-tests"))]
pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// 依次执行未应用的迁移；已应用的版本跳过（幂等）。
///
/// **降级护栏**：库的 `user_version` 比本程序支持的版本高时直接报错。那种情况
/// （换回旧版本、或同一数据目录被两个分支/两份安装共用）下，下面的 `skip` 会
/// 一次都不执行并返回 Ok，旧二进制继续对着新 schema 跑——表现是毫无头绪的查询
/// 错误（"no such column" 之类），真正的原因要翻很久。本仓已踩过一次
/// （跨分支共享数据目录）。
pub fn run(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > MIGRATIONS.len() as i64 {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "数据目录比本程序新（库 v{current}，本程序支持到 v{}）：请升级到较新版本，\
                 或换一个数据目录。",
                MIGRATIONS.len()
            )),
        ));
    }
    for (version, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        apply_migration(conn, version, sql)?;
    }
    Ok(())
}

/// 在事务内应用单个迁移：batch 中任一条语句失败则整体回滚（含 user_version）。
/// 事务守卫默认 Drop 即回滚，因此中途 `?` 返回会自动撤销已执行的语句。
pub fn apply_migration(conn: &mut Connection, version: usize, sql: &str) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(sql)?;
    tx.pragma_update(None, "user_version", version as i64 + 1)?;
    tx.commit()
}
