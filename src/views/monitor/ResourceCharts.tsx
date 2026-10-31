import { useEffect, useState } from 'react'
import { monitorResources } from '../../api'
import type { ServerResources } from '../../api/types'
import Sparkline from '../../components/charts/Sparkline'

const RANGES = [
  { secs: 3600, label: '1 小时' },
  { secs: 86400, label: '24 小时' },
  { secs: 604800, label: '7 天' },
]

export default function ResourceCharts({
  serverId,
  intervalSecs,
}: {
  serverId: number
  intervalSecs: number
}) {
  const [range, setRange] = useState(3600)
  const [data, setData] = useState<ServerResources | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let alive = true
    setError(null)
    monitorResources(serverId, range)
      .then((d) => {
        if (alive) setData(d)
      })
      .catch((e) => {
        if (alive) setError(String(e))
      })
    return () => {
      alive = false
    }
  }, [serverId, range])

  // 后端按 range/60 秒下采样，断线阈值按实际点距计算。
  const step = Math.max(intervalSecs, Math.ceil(range / 60))
  return (
    <div className="mt-3">
      <div className="flex justify-end gap-1">
        {RANGES.map((r) => (
          <button
            key={r.secs}
            type="button"
            onClick={() => setRange(r.secs)}
            className={`rounded px-2 py-0.5 text-xs ${
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
        <p className="mt-1 text-xs text-red-300">资源趋势加载失败：{error}</p>
      ) : (
        <div className="mt-1 grid grid-cols-1 gap-2 sm:grid-cols-3">
          <Sparkline
            label="CPU"
            points={data?.cpu_pct ?? []}
            intervalSecs={step}
            color="#34d399"
          />
          <Sparkline
            label="内存"
            points={data?.mem_pct ?? []}
            intervalSecs={step}
            color="#60a5fa"
          />
          <Sparkline
            label="磁盘"
            points={data?.disk_pct ?? []}
            intervalSecs={step}
            color="#f59e0b"
          />
        </div>
      )}
    </div>
  )
}
