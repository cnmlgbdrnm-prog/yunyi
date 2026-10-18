// 节点列表的「延迟」单元格：经代理的完整往返 + 中转收益 + 分跳裸 RTT。
//
// 三种数字**口径不同，不能相加**，这是本单元格唯一要守住的事（改文案前先读）：
// - 延迟：经代理的完整 HTTP 往返（mihomo URL 测试），≥2 个 RTT；
// - 中转收益：直连延迟 − 经中转延迟，两侧同口径才能相减，缺任一侧就不显示
//   （「没测过」和「直连失败」都不能谎报成 0 收益）；
// - 分跳：裸 TCP 1 个 RTT，只作「哪一跳烂」的定位，缺段用「—」占位保持位置固定。
import type { Node } from '../api/types'

const HOP_TITLE =
  '分跳裸 TCP 往返：本机→中转机 / 本机→落地机 / 中转机→落地机（经 WireGuard 隧道），各 1 个往返。与上面的延迟口径不同（那是经代理的完整 HTTP 往返），三者相加 ≠ 延迟。'

interface Props {
  node: Node
}

export default function NodeLatencyCell({ node: n }: Props) {
  // 两侧同口径（都是 mihomo URL 测试）才能相减。缺任一侧就不显示；
  // 摘掉中转后旧对比值也不再适用（relay_rule_id 为空即无中转可谈）
  const relayGain =
    n.relay_rule_id != null && n.latency != null && n.direct_latency != null
      ? n.direct_latency - n.latency
      : null
  const hopParts: Array<[string, number | null]> = [
    ['本→中', n.hop_relay_ms],
    ['本→落', n.hop_landing_ms],
    ['中→落', n.hop_tunnel_ms],
  ]
  const hasHops = hopParts.some(([, v]) => v != null)
  return (
    <td className="px-3 py-2 text-neutral-400">
      <div>{n.latency != null ? `${n.latency}ms` : '-'}</div>
      {relayGain != null && (
        <div className="text-xs text-neutral-500">
          直连 {n.direct_latency}ms ·{' '}
          {relayGain >= 0 ? `省 ${relayGain}ms` : `多 ${-relayGain}ms`}
        </div>
      )}
      {hasHops && (
        <div className="text-xs text-neutral-500" title={HOP_TITLE}>
          {hopParts.map(([label, v]) => `${label} ${v != null ? `${v}ms` : '—'}`).join(' · ')}
        </div>
      )}
    </td>
  )
}
