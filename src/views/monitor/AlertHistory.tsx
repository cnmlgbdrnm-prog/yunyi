import { useCallback, useEffect, useState } from 'react'
import { monitorAlerts } from '../../api'
import type { MonitorAlert } from '../../api/types'

export default function AlertHistory({
  refreshKey,
  onAck,
}: {
  refreshKey: number
  onAck: (id: number) => void
}) {
  const [includeResolved, setIncludeResolved] = useState(false)
  const [alerts, setAlerts] = useState<MonitorAlert[]>([])
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(() => {
    monitorAlerts(includeResolved, 100)
      .then(setAlerts)
      .catch((e) => setError(String(e)))
  }, [includeResolved])

  useEffect(() => {
    load()
  }, [load, refreshKey])

  return (
    <section className="rounded border border-neutral-800 bg-neutral-900 p-4">
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium">告警历史</h3>
        <label className="flex items-center gap-2 text-xs text-neutral-400">
          <input
            type="checkbox"
            checked={includeResolved}
            onChange={(e) => setIncludeResolved(e.target.checked)}
          />
          包含已恢复
        </label>
      </div>
      {error && <p className="mt-2 text-xs text-red-300">{error}</p>}
      {alerts.length === 0 ? (
        <p className="mt-3 text-xs text-neutral-500">暂无告警</p>
      ) : (
        <div className="mt-3 overflow-x-auto">
          <table className="w-full text-left text-xs">
            <thead className="text-neutral-500">
              <tr>
                <th className="py-1 pr-3">级别</th>
                <th className="py-1 pr-3">服务器</th>
                <th className="py-1 pr-3">说明</th>
                <th className="py-1 pr-3">最近值</th>
                <th className="py-1 pr-3">时间</th>
                <th className="py-1">操作</th>
              </tr>
            </thead>
            <tbody>
              {alerts.map((a) => (
                <tr key={a.id} className="border-t border-neutral-800">
                  <td className={a.severity === 'critical' ? 'text-red-300' : 'text-amber-300'}>
                    {a.severity === 'critical' ? '严重' : '警告'}
                  </td>
                  <td className="py-1 pr-3">{a.server_name}</td>
                  <td className="py-1 pr-3">{a.message}</td>
                  <td className="py-1 pr-3 text-neutral-400">{a.last_value ?? '—'}</td>
                  <td className="py-1 pr-3 text-neutral-500">
                    {a.resolved_at ? `已恢复 ${new Date(a.resolved_at).toLocaleString()}` : new Date(a.opened_at).toLocaleString()}
                  </td>
                  <td className="py-1">
                    {a.resolved_at == null ? (
                      a.acked_at ? (
                        <span className="text-neutral-500">已确认</span>
                      ) : (
                        <button type="button" onClick={() => onAck(a.id)} className="text-emerald-400">
                          确认
                        </button>
                      )
                    ) : (
                      <span className="text-neutral-600">—</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  )
}
