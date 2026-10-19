import type { MonitorPoint } from '../../api/types'

const W = 360
const H = 120
const PAD = 8

export default function LineChart({
  points,
  intervalSecs,
  label,
  color = '#34d399',
  formatValue = (v) => v.toFixed(1),
}: {
  points: MonitorPoint[]
  intervalSecs: number
  label: string
  color?: string
  formatValue?: (v: number) => string
}) {
  if (points.length === 0) {
    return <p className="py-6 text-center text-sm text-neutral-500">暂无数据（应用运行时才会采集）</p>
  }

  const times = points.map((p) => new Date(p.ts).getTime())
  const values = points.map((p) => p.value)
  const start = Math.min(...times)
  const end = Math.max(...times)
  const span = Math.max(end - start, 1)
  const min = Math.min(...values)
  const max = Math.max(...values)
  const pad = max - min === 0 ? 1 : (max - min) * 0.1
  const lo = min - pad
  const hi = max + pad

  const x = (t: number) => PAD + ((t - start) / span) * (W - PAD * 2)
  const y = (v: number) => H - PAD - ((v - lo) / (hi - lo)) * (H - PAD * 2)

  const segments: string[] = []
  let d = ''
  for (let i = 0; i < points.length; i++) {
    const gap =
      i > 0 && times[i] - times[i - 1] > Math.max(intervalSecs * 2, 1) * 1000
    if (gap) {
      if (d) segments.push(d)
      d = ''
    }
    d += `${d ? 'L' : 'M'}${x(times[i]).toFixed(1)},${y(values[i]).toFixed(1)}`
  }
  if (d) segments.push(d)

  const last = points[points.length - 1]
  const fmtTime = (t: number) =>
    span > 2 * 3600_000
      ? new Date(t).toLocaleString([], {
          month: '2-digit',
          day: '2-digit',
          hour: '2-digit',
          minute: '2-digit',
        })
      : new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })

  return (
    <div>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        className="w-full"
        role="img"
        aria-label={`${label} 趋势，${fmtTime(start)} 至 ${fmtTime(end)}`}
      >
        <title>{`${label} 趋势`}</title>
        {[0, 0.5, 1].map((f) => {
          const gy = PAD + f * (H - PAD * 2)
          return (
            <line
              key={f}
              x1={PAD}
              x2={W - PAD}
              y1={gy}
              y2={gy}
              stroke="#262626"
              strokeWidth="1"
              vectorEffect="non-scaling-stroke"
            />
          )
        })}
        {segments.map((seg, i) => (
          <path
            key={i}
            d={seg}
            fill="none"
            stroke={color}
            strokeWidth="1.5"
            vectorEffect="non-scaling-stroke"
          />
        ))}
      </svg>
      <div className="flex justify-between text-xs text-neutral-500">
        <span>{fmtTime(start)}</span>
        <span>{fmtTime(end)}</span>
      </div>
      <p className="mt-1 text-xs text-neutral-400">
        最新 {formatValue(last.value)} · 最小 {formatValue(min)} · 最大 {formatValue(max)}
      </p>
    </div>
  )
}
