// 分享创建任务的全局提示：切到其他视图后仍能看见任务在跑/已生成/失败。
// 状态在 shareCreationStore（模块级），本组件只负责展示与「查看」跳转。
import {
  dismissShareCreation,
  useShareCreation,
} from '../views/shareCreationStore'

interface Props {
  /** 跳到分享页：那里有完整结果与失败原因。 */
  onOpen: () => void
}

export default function ShareCreationNotice({ onOpen }: Props) {
  const creation = useShareCreation()
  if (creation == null) return null
  const running = creation.status === 'running'
  return (
    <div
      role={creation.status === 'error' ? 'alert' : 'status'}
      className={`fixed bottom-4 right-4 z-50 flex max-w-sm items-start gap-2 rounded border px-3 py-2 text-sm shadow-lg ${
        creation.status === 'error'
          ? 'border-red-800 bg-red-950 text-red-200'
          : creation.status === 'done'
            ? 'border-emerald-800 bg-emerald-950 text-emerald-200'
            : 'border-sky-800 bg-sky-950 text-sky-200'
      }`}
    >
      <p className="min-w-0 flex-1">
        {running && `正在生成分享（${creation.serverName}）…`}
        {creation.status === 'done' && `分享已生成（${creation.serverName}）`}
        {creation.status === 'error' && `生成分享失败（${creation.serverName}）`}
      </p>
      <button
        type="button"
        onClick={onOpen}
        className="rounded bg-neutral-800 px-2 py-0.5 text-xs text-neutral-100 hover:bg-neutral-700"
      >
        查看
      </button>
      {!running && (
        <button
          type="button"
          aria-label="关闭生成分享提示"
          onClick={dismissShareCreation}
          className="rounded px-1 text-xs text-neutral-400 hover:text-neutral-200"
        >
          ×
        </button>
      )}
    </div>
  )
}
