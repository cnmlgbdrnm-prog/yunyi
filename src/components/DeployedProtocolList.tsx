// 已部署协议徽章：显示完整协议变体（如 VLESS + REALITY + Vision）与端口。
// 只显示裸协议名会让用户分不清同名的不同传输层/安全层组合。
// 每个协议单独一行：挤成一行时长变体名（VMess + WebSocket…）会折行，
// 反而看不出哪一段属于哪个协议。
import type { DeployedProtocolDetail } from '../api/types'

interface Props {
  protocols: DeployedProtocolDetail[]
}

export default function DeployedProtocolList({ protocols }: Props) {
  if (protocols.length === 0) return null
  return (
    <div className="text-xs text-neutral-400">
      <p>已部署协议：</p>
      <ul className="mt-0.5 list-none space-y-0.5 p-0">
        {protocols.map((p) => (
          <li key={`${p.protocol}:${p.port}`}>
            <span className="text-emerald-400">{p.label}</span>
            <span className="text-neutral-500">:{p.port}</span>
          </li>
        ))}
      </ul>
    </div>
  )
}
