import type { CarrierHealth, CarrierStats } from '../../api/types'

// 展示顺序对齐参考截图：联通红、电信蓝、移动绿；后端固定按 ct/cu/cm 返回。
const DISPLAY = [
  { key: 'cu', name: '联通', dot: 'bg-red-500' },
  { key: 'ct', name: '电信', dot: 'bg-blue-500' },
  { key: 'cm', name: '移动', dot: 'bg-emerald-500' },
]

const LEVEL_BG: Record<number, string> = {
  1: 'bg-emerald-500',
  2: 'bg-lime-500',
  3: 'bg-amber-500',
  4: 'bg-orange-500',
  5: 'bg-red-500',
}

const LEVEL_TEXT: Record<number, string> = {
  1: 'text-emerald-300',
  2: 'text-lime-300',
  3: 'text-amber-300',
  4: 'text-orange-300',
  5: 'text-red-300',
}

function Cell({
  ok,
  level,
  mode,
}: {
  ok: boolean
  level: number | null
  mode: 'latency' | 'loss'
}) {
  const cls =
    mode === 'loss'
      ? ok
        ? 'bg-emerald-500'
        : 'bg-red-700'
      : ok
        ? LEVEL_BG[level ?? 5]
        : 'bg-red-700'
  return <span className={`h-2 flex-1 rounded-sm ${cls}`} />
}

function Row({
  stat,
  name,
  dot,
  barCount,
  mode,
}: {
  stat: CarrierStats
  name: string
  dot: string
  barCount: number
  mode: 'latency' | 'loss'
}) {
  const value =
    mode === 'latency'
      ? stat.avg_ms == null
        ? '—'
        : `${stat.avg_ms.toFixed(1)} ms`
      : stat.loss_pct == null
        ? '—'
        : `${stat.loss_pct.toFixed(1)}%`
  const level = mode === 'latency' ? stat.avg_level : stat.loss_level
  const pad = Math.max(barCount - stat.bars.length, 0)
  return (
    <div
      className="flex items-center gap-2 text-xs"
      aria-label={`${name}${mode === 'latency' ? '延迟' : '丢包'} ${value}`}
    >
      <span className={`h-2 w-2 shrink-0 rounded-full ${dot}`} />
      <span className="w-8 shrink-0 text-neutral-400">{name}</span>
      <span
        className={`w-16 shrink-0 text-right ${level ? LEVEL_TEXT[level] : 'text-neutral-400'}`}
      >
        {value}
      </span>
      <span className="ml-1 flex min-w-0 flex-1 gap-0.5">
        {Array.from({ length: pad }, (_, i) => (
          <span key={`p${i}`} className="h-2 flex-1 rounded-sm bg-neutral-800" />
        ))}
        {stat.bars.map((b, i) => (
          <Cell key={b.ts + i} ok={b.ok} level={b.latency_level} mode={mode} />
        ))}
      </span>
    </div>
  )
}

const EMPTY: (key: string) => CarrierStats = (key) => ({
  key,
  avg_ms: null,
  avg_level: null,
  loss_pct: null,
  loss_level: null,
  samples: 0,
  bars: [],
})

export default function CarrierPanel({
  carrier,
  onOpenSettings,
}: {
  carrier: CarrierHealth | null
  onOpenSettings: () => void
}) {
  if (!carrier) {
    return (
      <section className="mt-3 rounded border border-neutral-800 p-3">
        <p className="text-xs text-neutral-400">未选择三网地区</p>
        <button
          type="button"
          onClick={onOpenSettings}
          className="mt-1 text-xs text-emerald-400 underline"
        >
          去设置
        </button>
      </section>
    )
  }
  const empty = carrier.carriers.every((c) => c.samples === 0)
  const stats = (key: string) =>
    carrier.carriers.find((c) => c.key === key) ?? EMPTY(key)
  return (
    <section className="mt-3 rounded border border-neutral-800 p-3">
      <p className="mb-1 text-xs text-neutral-500">三网 · {carrier.region_name}</p>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div>
          <h4 className="mb-1 text-xs font-medium text-neutral-300">延迟</h4>
          <div className="space-y-1">
            {DISPLAY.map((d) => (
              <Row
                key={d.key}
                stat={stats(d.key)}
                name={d.name}
                dot={d.dot}
                barCount={carrier.bar_count}
                mode="latency"
              />
            ))}
          </div>
        </div>
        <div>
          <h4 className="mb-1 text-xs font-medium text-neutral-300">丢包</h4>
          <div className="space-y-1">
            {DISPLAY.map((d) => (
              <Row
                key={d.key}
                stat={stats(d.key)}
                name={d.name}
                dot={d.dot}
                barCount={carrier.bar_count}
                mode="loss"
              />
            ))}
          </div>
        </div>
      </div>
      {empty && <p className="mt-2 text-xs text-neutral-500">等待首次采集</p>}
    </section>
  )
}
