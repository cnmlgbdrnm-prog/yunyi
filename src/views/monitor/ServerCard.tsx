import type { MonitorConfig, ServerHealth } from '../../api/types'
import TrendPanel from './TrendPanel'

function pct(used: number | null, total: number | null): number | null {
  if (used == null || total == null || total <= 0) return null
  return (used / total) * 100
}

function barColor(v: number | null, warning = 85, critical = 95): string {
  if (v == null) return 'bg-neutral-700'
  if (v >= critical) return 'bg-red-500'
  if (v >= warning) return 'bg-amber-500'
  return 'bg-emerald-500'
}

function fmtUptime(secs: number | null): string {
  if (secs == null) return '—'
  const d = Math.floor(secs / 86400)
  const h = Math.floor((secs % 86400) / 3600)
  return d > 0 ? `${d} 天 ${h} 小时` : `${h} 小时`
}

function relative(ts: string | null): string {
  if (!ts) return '暂无样本'
  const diff = Date.now() - new Date(ts).getTime()
  const min = Math.floor(diff / 60000)
  if (min < 1) return '刚刚'
  if (min < 60) return `${min} 分钟前`
  return `${Math.floor(min / 60)} 小时前`
}

function MetricBar({
  label,
  value,
  warning,
  critical,
}: {
  label: string
  value: number | null
  warning?: number
  critical?: number
}) {
  return (
    <div>
      <div className="flex justify-between text-xs text-neutral-400">
        <span>{label}</span>
        <span>{value == null ? '未知' : `${value.toFixed(1)}%`}</span>
      </div>
      <div className="mt-1 h-1.5 overflow-hidden rounded bg-neutral-800">
        <div
          className={`h-full ${barColor(value, warning, critical)}`}
          style={{ width: `${Math.min(Math.max(value ?? 0, 0), 100)}%` }}
        />
      </div>
    </div>
  )
}

export default function ServerCard({
  health,
  config,
  expanded,
  onToggle,
}: {
  health: ServerHealth
  config: MonitorConfig
  expanded: boolean
  onToggle: () => void
}) {
  const s = health.sample
  const cpu = s?.cpu_pct ?? null
  const mem = pct(s?.mem_used_kb ?? null, s?.mem_total_kb ?? null)
  const disk = pct(s?.disk_used_kb ?? null, s?.disk_total_kb ?? null)
  const loadThreshold = (s?.cpu_cores ?? 0) * config.defaults.load_per_core
  const paused = !health.monitored

  const stateText = paused
    ? '已暂停监控'
    : health.online === true
      ? '在线'
      : health.online === false
        ? '不可达'
        : '暂无数据'

  return (
    <div className="rounded border border-neutral-800 bg-neutral-900 p-4">
      <button type="button" onClick={onToggle} className="w-full text-left">
        <div className="flex items-start justify-between gap-2">
          <div>
            <p className="font-medium">{health.name}</p>
            <p className="text-xs text-neutral-500">{health.host}</p>
          </div>
          <div className="text-right text-xs">
            <p
              className={
                paused
                  ? 'text-neutral-500'
                  : health.online === true
                    ? 'text-emerald-400'
                    : health.online === false
                      ? 'text-red-400'
                      : 'text-neutral-500'
              }
            >
              {stateText}
            </p>
            <p className="text-neutral-500">
              {health.stale ? '数据已过期 · ' : ''}
              {relative(health.last_sample_at)}
            </p>
          </div>
        </div>

        <div className="mt-3 grid grid-cols-1 gap-2 sm:grid-cols-3">
          <MetricBar label="CPU（约 1 秒）" value={cpu} />
          <MetricBar label="内存" value={mem} />
          <MetricBar label="磁盘 /" value={disk} warning={config.defaults.disk_pct} critical={config.defaults.disk_critical_pct} />
        </div>

        <div className="mt-3 grid grid-cols-2 gap-1 text-xs text-neutral-400 sm:grid-cols-3">
          <span>负载 {s?.load1?.toFixed(2) ?? '—'} / {s?.cpu_cores ?? '—'} 核</span>
          <span>负载阈值 {loadThreshold.toFixed(1)}</span>
          <span>运行 {fmtUptime(s?.uptime_secs ?? null)}</span>
          <span>连接 {s?.connect_ms != null ? `${s.connect_ms} ms` : '—'}</span>
          <span>BBR {s?.bbr_algo ?? '—'}</span>
          <span>核数 {s?.cpu_cores ?? '—'}</span>
        </div>

        <div className="mt-3 flex flex-wrap gap-1 text-xs">
          {['xray', 'ssserver', 'singbox'].map((unit) => {
            const expected = health.expected_services.includes(unit)
            const state = s?.services?.[unit] ?? 'unknown'
            const cls = !expected
              ? health.deployment_known
                ? 'bg-neutral-800 text-neutral-500'
                : 'bg-neutral-800 text-neutral-400'
              : state === 'active'
                ? 'bg-emerald-950 text-emerald-300'
                : state === 'inactive' || state === 'failed'
                  ? 'bg-red-950 text-red-300'
                  : 'bg-neutral-800 text-neutral-400'
            return (
              <span key={unit} className={`rounded px-2 py-0.5 ${cls}`}>
                {unit} {health.deployment_known ? (expected ? state : '未部署') : state}
              </span>
            )
          })}
          {health.wg_expected && (
            <span className="rounded bg-neutral-800 px-2 py-0.5 text-neutral-300">
              WG {s?.wg_interfaces == null ? '未知' : s.wg_interfaces.length ? s.wg_interfaces.join(', ') : '未发现接口'}
            </span>
          )}
        </div>

        {(s?.error || health.local_error) && (
          <p className="mt-2 truncate text-xs text-red-300" title={s?.error ?? health.local_error ?? ''}>
            {s?.error ?? health.local_error}
          </p>
        )}
      </button>
      {expanded && (
        <TrendPanel serverId={health.server_id} intervalSecs={config.interval_secs} />
      )}
    </div>
  )
}
