import { useState, type ChangeEvent } from 'react'
import { monitorSetConfig } from '../../api'
import type { MonitorConfig, MonitorThresholds, ServerHealth } from '../../api/types'

const RESET: MonitorThresholds = {
  cpu_pct: 90, mem_pct: 90, disk_pct: 85, disk_critical_pct: 95,
  load_per_core: 2, breach_samples: 3, clear_samples: 2, offline_samples: 3,
}

export default function MonitorSettingsDrawer({
  config,
  servers,
  onClose,
  onSaved,
}: {
  config: MonitorConfig
  servers: ServerHealth[]
  onClose: () => void
  onSaved: (c: MonitorConfig) => void
}) {
  const [enabled, setEnabled] = useState(config.enabled)
  const [interval, setIntervalSecs] = useState(config.interval_secs)
  const [t, setT] = useState<MonitorThresholds>({ ...config.defaults })
  const [serverOn, setServerOn] = useState<Record<number, boolean>>(() =>
    Object.fromEntries(
      servers.map((s) => [s.server_id, config.servers[String(s.server_id)]?.enabled !== false]),
    ),
  )
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const num = (key: keyof MonitorThresholds) => ({
    value: t[key],
    onChange: (e: ChangeEvent<HTMLInputElement>) =>
      setT((prev) => ({ ...prev, [key]: e.target.valueAsNumber })),
  })

  const validate = (): string | null => {
    if (!(interval >= 30 && interval <= 3600)) return '采集间隔必须在 30–3600 秒之间'
    for (const k of ['cpu_pct', 'mem_pct', 'disk_pct', 'disk_critical_pct'] as const) {
      if (!(t[k] >= 1 && t[k] <= 100)) return '百分比阈值必须在 1–100 之间'
    }
    if (!(t.disk_critical_pct > t.disk_pct)) return '磁盘严重阈值必须高于警告阈值'
    if (!(t.load_per_core >= 0.1 && t.load_per_core <= 100)) return '负载阈值必须在 0.1–100 之间'
    for (const k of ['breach_samples', 'clear_samples', 'offline_samples'] as const) {
      if (!(t[k] >= 1 && t[k] <= 10)) return '连续次数必须在 1–10 之间'
    }
    return null
  }

  const save = async () => {
    const msg = validate()
    if (msg) {
      setError(msg)
      return
    }
    setBusy(true)
    setError(null)
    try {
      const next: MonitorConfig = {
        ...config,
        enabled,
        interval_secs: interval,
        defaults: { ...t },
        servers: Object.fromEntries(
          servers.map((s) => {
            const prev = config.servers[String(s.server_id)] ?? {}
            return [String(s.server_id), { ...prev, enabled: serverOn[s.server_id] }]
          }),
        ),
      }
      onSaved(await monitorSetConfig(next))
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  const field = (label: string, key: keyof MonitorThresholds, step = '1') => (
    <label className="block text-xs text-neutral-400">
      {label}
      <input
        type="number"
        step={step}
        {...num(key)}
        aria-label={label}
        className="mt-1 w-full rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm text-neutral-100"
      />
    </label>
  )

  return (
    <div className="fixed inset-y-0 right-0 z-20 w-96 overflow-y-auto border-l border-neutral-800 bg-neutral-950 p-4 shadow-xl">
      <div className="flex items-center justify-between">
        <h3 className="font-medium">监控设置</h3>
        <button type="button" onClick={onClose} className="text-neutral-400">
          关闭
        </button>
      </div>
      <div className="mt-4 space-y-3">
        <label className="flex items-center gap-2 text-sm">
          <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
          自动监控（应用打开时）
        </label>
        <label className="block text-xs text-neutral-400">
          采集间隔（秒）
          <input
            type="number"
            min={30}
            max={3600}
            value={interval}
            onChange={(e) => setIntervalSecs(e.target.valueAsNumber)}
            aria-label="采集间隔"
            className="mt-1 w-full rounded border border-neutral-700 bg-neutral-800 px-2 py-1 text-sm text-neutral-100"
          />
        </label>
        <div className="grid grid-cols-2 gap-2">
          {field('CPU 阈值', 'cpu_pct')}
          {field('内存阈值', 'mem_pct')}
          {field('磁盘警告阈值', 'disk_pct')}
          {field('磁盘严重阈值', 'disk_critical_pct')}
          {field('负载（每核）', 'load_per_core', '0.1')}
          {field('连续越界次数', 'breach_samples')}
          {field('连续恢复次数', 'clear_samples')}
          {field('离线判定次数', 'offline_samples')}
        </div>
        <div>
          <p className="text-xs text-neutral-400">服务器</p>
          {servers.map((s) => (
            <label key={s.server_id} className="mt-1 flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={serverOn[s.server_id] ?? true}
                onChange={(e) =>
                  setServerOn((prev) => ({ ...prev, [s.server_id]: e.target.checked }))
                }
              />
              {s.name}
            </label>
          ))}
        </div>
        {error && <p className="text-xs text-red-300">{error}</p>}
        <div className="flex justify-between">
          <button
            type="button"
            onClick={() => setT({ ...RESET })}
            className="rounded border border-neutral-700 px-3 py-1.5 text-sm text-neutral-300"
          >
            恢复默认
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => void save()}
            className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white disabled:opacity-50"
          >
            保存
          </button>
        </div>
      </div>
    </div>
  )
}
