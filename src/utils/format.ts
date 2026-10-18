// 展示格式化工具：传输字节（KB/MB/GB）与握手时间（秒/分钟/小时前）。
// 输入原始数值或 LinkStatus 握手字段；输出用户可见中文文案，纯函数无副作用。
import type { LinkStatus } from '../api/types'

export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return '0 B'
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`
  return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`
}

/**
 * 测速带宽 → 展示文本。输入单位是 **bytes/s**（`speed::download_aggregate` 返回
 * Σ已下载字节/墙钟，节点与服务器表的 bandwidth 列同源），按 1MiB=1048576 换算。
 *
 * 注意别与 Dashboard 的 throughput 混用：那张表来自 iperf3 的 `bits_per_second`，
 * 单位是 **bit/s**，走 `mbpsText`。两者单位不同，曾因此误判成「差 8 倍」。
 */
export function formatBandwidth(bytesPerSec: number): string {
  return `${(bytesPerSec / 1048576).toFixed(2)} MB/s`
}

export function formatHandshakeAgo(secs: number | null): string {
  if (secs == null) return '未握手'
  if (secs < 60) return `${secs} 秒前`
  if (secs < 3600) return `${Math.floor(secs / 60)} 分钟前`
  return `${Math.floor(secs / 3600)} 小时前`
}

// 取两端最近的握手时间（秒数最小者）；都无则 null
export function handshakeSecs(st: LinkStatus): number | null {
  const candidates = [st.relay_handshake_secs_ago, st.landing_handshake_secs_ago].filter((v): v is number => v != null)
  return candidates.length === 0 ? null : Math.min(...candidates)
}
