// 上游出口列表的一行：名称/协议/地址、行内改监听端口、编辑/探测/重部署/删除。
import type { Upstream } from '../../api/types'

export default function UpstreamRow({
  u,
  editingValue,
  busy,
  stale,
  onEditingValueChange,
  onSavePort,
  onStartEditPort,
  onCancelEditPort,
  onEdit,
  onProbe,
  onRedeploy,
  onRemove,
}: {
  u: Upstream
  /** 非 null = 这一行正在行内编辑监听端口 */
  editingValue: string | null
  busy: boolean
  /** 改过但还没重新部署：远端仍是老配置，行上留痕 */
  stale: boolean
  onEditingValueChange: (value: string) => void
  onSavePort: () => void
  onStartEditPort: () => void
  onCancelEditPort: () => void
  onEdit: () => void
  onProbe: () => void
  onRedeploy: () => void
  onRemove: () => void
}) {
  return (
    <li className="rounded bg-neutral-900 px-2 py-1.5">
      <div className="flex items-center justify-between gap-2">
        <p className="truncate text-sm">
          {u.name}{' '}
          <span className="text-xs text-neutral-500">
            {u.proto} {u.host}:{u.port} ·{' '}
            {editingValue != null ? (
              <>
                监听{' '}
                <input
                  aria-label="监听端口"
                  className="w-20 rounded border border-neutral-700 bg-neutral-900 px-1 py-0.5 text-xs"
                  value={editingValue}
                  onChange={(e) => onEditingValueChange(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') onSavePort()
                    if (e.key === 'Escape') onCancelEditPort()
                  }}
                />
                <button
                  type="button"
                  disabled={busy}
                  onClick={onSavePort}
                  className="ml-1 rounded bg-neutral-800 px-1.5 py-0.5 text-emerald-300 hover:bg-neutral-700 disabled:opacity-50"
                >
                  保存
                </button>
                <button
                  type="button"
                  onClick={onCancelEditPort}
                  className="ml-1 text-neutral-400 hover:text-neutral-200"
                >
                  取消
                </button>
              </>
            ) : (
              <>
                监听{' '}
                <button
                  type="button"
                  aria-label={`改监听端口 ${u.listen_port}`}
                  onClick={onStartEditPort}
                  className="underline decoration-dotted hover:text-neutral-300"
                >
                  {u.listen_port}
                </button>
                {stale && <span className="ml-1 text-amber-400">（已改，重新部署后生效）</span>}
              </>
            )}
          </span>
        </p>
        <div className="flex shrink-0 items-center gap-2">
          <button
            type="button"
            aria-label={`编辑 ${u.name}`}
            disabled={busy}
            onClick={onEdit}
            className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-200 hover:bg-neutral-700 disabled:opacity-50"
          >
            编辑
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={onProbe}
            className="rounded bg-neutral-800 px-2 py-1 text-xs text-emerald-300 hover:bg-neutral-700 disabled:opacity-50"
          >
            探测出口 IP
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={onRedeploy}
            className="rounded bg-neutral-800 px-2 py-1 text-xs text-sky-300 hover:bg-neutral-700 disabled:opacity-50"
          >
            重新部署
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={onRemove}
            className="rounded bg-neutral-800 px-2 py-1 text-xs text-red-400 hover:bg-neutral-700 disabled:opacity-50"
          >
            删除
          </button>
        </div>
      </div>
      {u.egress_ip && <p className="mt-0.5 text-xs text-emerald-400">出口 IP {u.egress_ip}</p>}
      {u.last_check_error && <p className="mt-0.5 text-xs text-red-400">{u.last_check_error}</p>}
    </li>
  )
}
