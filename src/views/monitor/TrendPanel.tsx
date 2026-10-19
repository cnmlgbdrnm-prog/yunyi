import { useEffect, useState } from 'react'
import { monitorHistory } from '../../api'
import type { MonitorMetric, MonitorPoint } from '../../api/types'
import LineChart from '../../components/charts/LineChart'

const METRICS: { key: MonitorMetric; label: string; format: (v: number) => string }[] = [
  { key: 'cpu_pct', label: 'CPU', format: (v) => `${v.toFixed(1)}%` },
  { key: 'mem_pct', label: '内存', format: (v) => `${v.toFixed(1)}%` },
  { key: 'disk_pct', label: '磁盘', format: (v) => `${v.toFixed(1)}%` },
  { key: 'load1', label: '负载', format: (v) => v.toFixed(2) },
  { key: 'connect_ms', label: '连接耗时', format: (v) => `${Math.round(v)} ms` },
]

const RANGES = [
  { secs: 3600, label: '1 小时' },
  { secs: 86400, label: '24 小时' },
  { secs: 604800, label: '7 天' },
]

export default function TrendPanel({
  serverId,
  intervalSecs,
}: {
  serverId: number
  intervalSecs: number
}) {
  const [metric, setMetric] = useState<MonitorMetric>('cpu_pct')
  const [range, setRange] = useState(3600)
  const [points, setPoints] = useState<MonitorPoint[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    setLoading(true)
    setError(null)
    monitorHistory(serverId, metric, range)
      .then((p) => {
        if (alive) setPoints(p)
      })
      .catch((e) => {
        if (alive) setError(String(e))
      })
      .finally(() => {
        if (alive) setLoading(false)
      })
    return () => {
      alive = false
    }
  }, [serverId, metric, range])

  const meta = METRICS.find((m) => m.key === metric)!

  return (
    <div className="mt-3 border-t border-neutral-800 pt-3">
      <div className="mb-2 flex flex-wrap gap-1">
        {METRICS.map((m) => (
          <button
            key={m.key}
            type="button"
            onClick={() => setMetric(m.key)}
            className={`rounded px-2 py-1 text-xs ${
              metric === m.key
                ? 'bg-neutral-700 text-white'
                : 'text-neutral-400 hover:bg-neutral-800'
            }`}
          >
            {m.label}
          </button>
        ))}
        <span className="mx-1 w-px bg-neutral-800" />
        {RANGES.map((r) => (
          <button
            key={r.secs}
            type="button"
            onClick={() => setRange(r.secs)}
            className={`rounded px-2 py-1 text-xs ${
              range === r.secs
                ? 'bg-neutral-700 text-white'
                : 'text-neutral-400 hover:bg-neutral-800'
            }`}
          >
            {r.label}
          </button>
        ))}
      </div>
      {error ? (
        <p className="text-xs text-red-300">趋势加载失败：{error}</p>
      ) : loading ? (
        <p className="py-6 text-center text-xs text-neutral-500">加载中…</p>
      ) : (
        <LineChart
          points={points}
          intervalSecs={intervalSecs}
          label={meta.label}
          formatValue={meta.format}
        />
      )}
    </div>
  )
}
