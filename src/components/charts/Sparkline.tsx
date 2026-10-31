import type { MonitorPoint } from '../../api/types'

const W = 120
const H = 28
const PAD = 2

export default function Sparkline({
  label,
  points,
  intervalSecs,
  color = '#34d399',
  formatValue = (v) => `${v.toFixed(1)}%`,
}: {
  label: string
  points: MonitorPoint[]
  intervalSecs: number
  color?: string
  formatValue?: (v: number) => string
}) {
  const last = points[points.length - 1]
  if (!last) {
    return (
      <div className="rounded border border-neutral-800 p-2 text-xs text-neutral-500">
        <div className="flex justify-between">
          <span>{label}</span>
          <span>暂无数据</span>
        </div>
        <div className="mt-1 h-7" />
      </div>
    )
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

  return (
    <div className="rounded border border-neutral-800 p-2 text-xs">
      <div className="flex justify-between text-neutral-400">
        <span>{label}</span>
        <span className="text-neutral-200">{formatValue(last.value)}</span>
      </div>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        className="mt-1 w-full"
        role="img"
        aria-label={`${label} 迷你趋势`}
      >
        <title>{`${label} 迷你趋势`}</title>
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
    </div>
  )
}
