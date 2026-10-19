import { useEffect, useState } from 'react'
import { monitorAck, monitorCollect } from '../api'
import type { ViewKey } from '../App'
import {
  applySummary,
  refreshMonitor,
  useMonitorSummary,
} from '../monitor/registry'
import AlertHistory from './monitor/AlertHistory'
import MonitorSettingsDrawer from './monitor/MonitorSettingsDrawer'
import ServerCard from './monitor/ServerCard'

function relative(ts: string | null): string {
  if (!ts) return '尚未采集'
  const min = Math.floor((Date.now() - new Date(ts).getTime()) / 60000)
  if (min < 1) return '刚刚'
  if (min < 60) return `${min} 分钟前`
  return `${Math.floor(min / 60)} 小时前`
}

export default function Monitor({ onNavigate }: { onNavigate: (v: ViewKey) => void }) {
  const { summary, collecting, error } = useMonitorSummary()
  const [expanded, setExpanded] = useState<number | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [historyKey, setHistoryKey] = useState(0)

  useEffect(() => {
    void refreshMonitor()
  }, [])

  const collect = async () => {
    setActionError(null)
    try {
      applySummary(await monitorCollect())
    } catch (e) {
      setActionError(String(e))
    }
  }

  const ack = async (id: number) => {
    setActionError(null)
    try {
      await monitorAck(id)
      await refreshMonitor()
      setHistoryKey((k) => k + 1)
    } catch (e) {
      setActionError(String(e))
    }
  }

  if (!summary) {
    return (
      <div className="space-y-3">
        <h2 className="text-xl font-semibold">服务器监控</h2>
        <p className="text-sm text-neutral-400">{error ?? '正在加载监控摘要…'}</p>
      </div>
    )
  }

  const unacked = summary.active_alerts.filter((a) => a.acked_at == null).length

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-xl font-semibold">服务器监控</h2>
          <p className="text-xs text-neutral-500">
            上次成功采集：{relative(summary.last_round_at)} · 仅在应用打开时采集
          </p>
        </div>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => void collect()}
            disabled={collecting}
            className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white disabled:opacity-50"
          >
            {collecting ? '采集中…' : '立即采集'}
          </button>
          <button
            type="button"
            onClick={() => setSettingsOpen(true)}
            className="rounded border border-neutral-700 px-3 py-1.5 text-sm text-neutral-300"
          >
            设置
          </button>
        </div>
      </div>

      {(actionError || error) && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {actionError ?? error}
        </p>
      )}
      {summary.config_error && (
        <p className="rounded border border-amber-700 bg-amber-950 px-3 py-2 text-sm text-amber-200">
          监控配置无法解析，已使用默认值：{summary.config_error}
        </p>
      )}
      {!summary.config.enabled && (
        <p className="rounded border border-neutral-700 bg-neutral-900 px-3 py-2 text-sm text-neutral-300">
          自动监控已暂停，仍可手动采集。
        </p>
      )}
      {summary.last_round_error && (
        <p className="rounded border border-amber-700 bg-amber-950 px-3 py-2 text-sm text-amber-200">
          本轮采集失败：{summary.last_round_error}
        </p>
      )}
      {summary.active_alerts.length > 0 && (
        <section className="rounded border border-red-900 bg-red-950/40 p-3">
          <h3 className="text-sm font-medium text-red-200">
            活动告警 {summary.active_alerts.length} 条（未确认 {unacked} 条）
          </h3>
          <ul className="mt-2 space-y-1">
            {summary.active_alerts.map((a) => (
              <li key={a.id} className="flex items-start justify-between gap-2 text-xs">
                <span className={a.acked_at ? 'text-neutral-400' : 'text-red-200'}>
                  [{a.severity === 'critical' ? '严重' : '警告'}] {a.server_name}：{a.message}
                </span>
                {a.acked_at ? (
                  <span className="shrink-0 text-neutral-500">已确认</span>
                ) : (
                  <button
                    type="button"
                    onClick={() => void ack(a.id)}
                    className="shrink-0 rounded border border-red-700 px-2 py-0.5 text-red-200"
                  >
                    确认
                  </button>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}

      {summary.servers.length === 0 ? (
        <div className="rounded border border-neutral-800 bg-neutral-900 p-6 text-center text-sm text-neutral-400">
          还没有服务器，先去服务器页添加。
          <button
            type="button"
            onClick={() => onNavigate('servers')}
            className="ml-2 text-emerald-400 underline"
          >
            去服务器页
          </button>
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
          {summary.servers.map((h) => (
            <ServerCard
              key={h.server_id}
              health={h}
              config={summary.config}
              expanded={expanded === h.server_id}
              onToggle={() =>
                setExpanded((cur) => (cur === h.server_id ? null : h.server_id))
              }
            />
          ))}
        </div>
      )}

      <AlertHistory refreshKey={historyKey} onAck={(id) => void ack(id)} />
      {settingsOpen && (
        <MonitorSettingsDrawer
          config={summary.config}
          servers={summary.servers}
          onClose={() => setSettingsOpen(false)}
          onSaved={(cfg) => {
            void cfg
            setSettingsOpen(false)
            void refreshMonitor()
          }}
        />
      )}
    </div>
  )
}
