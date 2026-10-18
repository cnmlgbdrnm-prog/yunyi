// REALITY 伪装域名（dest / serverNames）候选清单 + 默认值。
//
// 机制先说清楚，免得被分组标签误导：dest **不承载用户流量**——你的数据走 vless/trojan
// 那条加密通道，dest 只参与两件事：①握手镜像（服务端把 ClientHello 转给 dest，再做
// key-share 替换）②未认证流量的透传。所以这里按**站点体量**分组，衡量的是「反代合理性」：
// 你的 IP 在给一个大站做反向代理，体量越大越不显得可疑——这正是 REALITY 抗主动探测的
// 原理，而不是「dest 得扛得住多少带宽」。选大站是对的，理由是合理性不是带宽。
//
// 清单来源：2026-09-18 在 ashura(203.0.113.17, Xray 26.3.27) 上逐个端到端实测——
// 服务器上给每个候选起一个独立 REALITY 入站，客户端经 mihomo（同款 client-fingerprint
// chrome + flow xtls-rprx-vision）真实握手并取回 gstatic 204。20 个候选 19 个通过，
// **唯一失败的是 www.microsoft.com**：服务端报 "REALITY: processed invalid connection
// ... handshake did not complete successfully"，客户端一律 EOF/503。同机的 www.bing.com
// （同为微软）却是通的，所以不是「CDN 不行」，是那个域名特有的，别照此推广结论。
//
// 注意实测口径：这一轮走的是 SSH 隧道（ashura 的 ufw 只放行生产端口），隧道把各家延迟
// 拉平到 600~700ms，**所以延迟数值不可用于横向比较**，能比的只有「通/不通」。
// 另外 www.nvidia.com 曾在真机上表现为「连不上」，端到端复测证明是客户端未同步旧
// SNI 所致，域名本身没问题——见下方 note 的措辞。

export interface RealityDestOption {
  domain: string
  /** 端到端实测是否握手成功（2026-09-18，ashura） */
  ok: boolean
  /** 附加说明：实测结论或已知代价，直接显示在下拉项里 */
  note?: string
  /** 需要醒目标色：实测失败，或 Xray 主动警告过 */
  warn?: boolean
}

export interface RealityDestGroup {
  label: string
  options: RealityDestOption[]
}

/** 苹果系目标的共同代价，来自 Xray 启动时的原话：
 * "Choosing apple, icloud, etc. as the target may get your IP blocked by the GFW"
 * ——太多人拿它当 dest，特征反而显著。可用，但别以为它是最稳的。 */
const APPLE_CAVEAT = 'Xray 警告：苹果系目标招 IP 封锁'

export const REALITY_DEST_GROUPS: RealityDestGroup[] = [
  {
    label: '大流量站（体量大，反代合理，推荐）',
    options: [
      { domain: 'www.apple.com', ok: true, warn: true, note: APPLE_CAVEAT },
      { domain: 'dl.google.com', ok: true },
      { domain: 'www.cloudflare.com', ok: true },
      { domain: 'www.amazon.com', ok: true },
      { domain: 'www.bing.com', ok: true },
      { domain: 'www.samsung.com', ok: true },
      { domain: 'www.tesla.com', ok: true },
      { domain: 'www.nvidia.com', ok: true },
      { domain: 'www.yahoo.com', ok: true },
      { domain: 'www.adobe.com', ok: true },
      { domain: 'www.oracle.com', ok: true },
      { domain: 'www.icloud.com', ok: true, warn: true, note: APPLE_CAVEAT },
      { domain: 'www.dropbox.com', ok: true },
      { domain: 'www.wikipedia.org', ok: true },
    ],
  },
  {
    label: '较小众站（体量小，大流量场景慎用）',
    options: [
      { domain: 'cdn.jsdelivr.net', ok: true },
      { domain: 'www.python.org', ok: true },
      { domain: 'www.docker.com', ok: true },
      { domain: 'gateway.icloud.com', ok: true, warn: true, note: APPLE_CAVEAT },
      { domain: 'swdist.apple.com', ok: true, warn: true, note: APPLE_CAVEAT },
    ],
  },
  {
    label: '实测握手失败（保留仅供对照）',
    options: [
      {
        domain: 'www.microsoft.com',
        ok: false,
        warn: true,
        note: '实测 REALITY 握手必失败：服务端报 invalid connection，客户端 EOF/503',
      },
    ],
  },
]

/** 扁平清单（只取域名）：供测试与「默认值必须在清单内」这类不变量校验使用。 */
export const ALL_REALITY_DESTS: string[] = REALITY_DEST_GROUPS.flatMap((g) =>
  g.options.map((o) => o.domain),
)

/** 查一个域名是不是清单里的预设项（下拉显示与输入框对齐用）。 */
export function findRealityDest(domain: string): RealityDestOption | undefined {
  const d = domain.trim()
  for (const g of REALITY_DEST_GROUPS) {
    const hit = g.options.find((o) => o.domain === d)
    if (hit) return hit
  }
  return undefined
}

/// 默认伪装域名。**这是产品决定，不是笔误**：它在 ashura 上实测必失败（见文件头）。
/// 换成已验证值只需改这一行，同时改后端同名常量
/// （src-tauri/src/servers/upstream.rs 的 DEFAULT_REALITY_DEST；deploy.rs 复用同一个）——
/// 两边漂移的表现是「界面显示 A、服务器写 B」，且失败只在客户端侧可见。
export const DEFAULT_REALITY_DEST = 'www.microsoft.com'
