// 节点列表的「隐私保护」单元格：三种可见形态（处理中 / 未开 / 已开或需处理）。
//
// 一段话讲清这个单元格要防的误读（改文案前先读）：
// **有记录但地址没回读 = 部署中断，此刻这条线仍走原入口**。它必须显示成
// 「未生效·重试」而不是「隐私✓」——开着不生效比没开更需要被看见，
// 所以未生效态用红底，而不是继承「已关闭」的中性色。
//
// 为什么单独成组件：这段判据原本是行内的 IIFE（Nodes.tsx 里的 `{(() => {…})()}`），
// 与表格其它单元格混在一起；拆出来后它有名字、有边界，加一种状态不必再动那张表。
import type { Privacy } from '../api/types'

interface Props {
  /** 该节点的隐私记录；undefined = 从没开过 */
  privacy: Privacy | undefined
  /** 有隐私任务在跑（部署中/关闭中）：作业没跑完前不给点，免得连点堆出一串任务 */
  busy: boolean
  onToggle: () => void
}

/** 一段话讲清开关的语义：隐藏服务开在**入口所在那台机器**上，协议与端口都不动 */
const OPEN_TITLE =
  '把这条线的入口换成 Tor 洋葱地址：入口在哪台机器上，隐藏服务就开在哪台（挂了中转就是中转机），协议与端口都不动'

export default function NodePrivacyCell({ privacy: p, busy, onToggle }: Props) {
  if (busy) {
    return (
      <span
        className="rounded bg-neutral-800 px-2 py-1 text-xs text-amber-300"
        title="正在那台服务器上部署 Tor 隐藏服务，地址回读到才算生效"
      >
        处理中…
      </span>
    )
  }
  if (!p) {
    return (
      <button
        type="button"
        onClick={onToggle}
        title={OPEN_TITLE}
        className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-400 hover:bg-neutral-700"
      >
        隐私
      </button>
    )
  }
  // 地址回读到了、且上一次核验没报错，才算真的生效
  const live = p.onion_host !== '' && !p.last_check_error
  return (
    <button
      type="button"
      onClick={onToggle}
      title={
        live
          ? `经 Tor 洋葱入口：${p.onion_host}\n隐藏服务在服务器上，转发到 ${p.fwd_host}:${p.fwd_port}`
          : p.onion_host
            ? `未确认生效；点击重试：${p.last_check_error}`
            : `尚未生效（这条线此刻仍走原入口）；点击重试${p.last_check_error ? `：${p.last_check_error}` : ''}`
      }
      className={`rounded px-2 py-1 text-xs hover:bg-neutral-700 ${
        live ? 'bg-purple-900 text-purple-200' : 'bg-red-950 text-red-300'
      }`}
    >
      {live ? '隐私✓' : p.onion_host ? '需处理' : '未生效·重试'}
    </button>
  )
}
