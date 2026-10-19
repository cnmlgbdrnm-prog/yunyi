import type { MonitorSummary } from '../../api/types'

export default function ServerHealthCard({
  summary,
  onClick,
}: {
  summary: MonitorSummary | null
  onClick: () => void
}) {
  const monitored = summary?.servers.filter((s) => s.monitored) ?? []
  const online = monitored.filter((s) => s.online === true).length
  const unacked =
    summary?.active_alerts.filter((a) => a.acked_at == null).length ?? 0
  const alert = Boolean(
    summary && (summary.config_error || summary.active_alerts.length > 0),
  )
  const sub =
    summary == null
      ? '尚未采集'
      : summary.config_error
        ? '监控配置异常'
        : summary.active_alerts.length > 0
          ? `${summary.active_alerts.length} 条活动告警（${unacked} 条未确认）`
          : '无活动告警'

  return (
    <button
      type="button"
      onClick={onClick}
      className={`rounded border p-4 text-left transition-colors hover:border-neutral-600 ${
        alert ? 'border-red-900 bg-red-950' : 'border-neutral-800 bg-neutral-900'
      }`}
    >
      <p className="text-xs text-neutral-400">服务器健康</p>
      <p className="mt-1 text-2xl font-semibold">
        {summary ? `${online}/${monitored.length}` : '—'}
      </p>
      <p className="mt-1 text-xs text-neutral-500">{sub}</p>
    </button>
  )
}
