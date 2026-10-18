// 「自动探测」按钮：编辑已有服务器时从机器自身读取全局 IPv6（SSH 走原始主机地址）。
// 探测要连服务器，最长十几秒；忙态与结果只显示在按钮旁，不打断表单其他字段。
import { useState } from 'react'
import { detectServerIpv6 } from '../api'

interface Props {
  serverId: number
  /** 探测成功回调：推荐地址（候选首位），父级写进表单等待用户保存 */
  onDetected: (address: string) => void
}

export default function Ipv6DetectButton({ serverId, onDetected }: Props) {
  const [busy, setBusy] = useState(false)
  const [hint, setHint] = useState<{ ok: boolean; text: string } | null>(null)
  const run = async () => {
    setBusy(true)
    setHint(null)
    try {
      const list = await detectServerIpv6(serverId)
      onDetected(list[0])
      setHint({
        ok: true,
        text:
          list.length > 1
            ? `已填入推荐地址；另有 ${list.length - 1} 个候选：${list.slice(1).join('、')}`
            : '已探测并填入',
      })
    } catch (e) {
      setHint({ ok: false, text: String(e) })
    } finally {
      setBusy(false)
    }
  }
  return (
    <span className="inline-flex flex-wrap items-center gap-2 text-xs font-normal">
      <button
        type="button"
        disabled={busy}
        onClick={() => void run()}
        className="rounded bg-neutral-800 px-2 py-0.5 text-sky-300 hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
      >
        {busy ? '探测中…' : '自动探测'}
      </button>
      {hint && <span className={hint.ok ? 'text-emerald-400' : 'text-red-400'}>{hint.text}</span>}
    </span>
  )
}
