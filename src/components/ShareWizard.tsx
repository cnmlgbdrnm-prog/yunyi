// 新建分享向导：服务器 → 准入候选 → HTTPS 状态 → 配额/速率/到期 → 创建。
// 准入以后端 `listShareCandidates` 为准；前端只负责置灰与展示原因，不自行判断。
import { useEffect, useState } from 'react'
import {
  createShare,
  listShareCandidates,
  prepareShareTls,
  shareTlsStatus,
} from '../api'
import type { Server, ShareCandidate, ShareInfo, ShareTlsStatus } from '../api/types'

interface Props {
  servers: Server[]
  onClose: () => void
  onCreated: (info: ShareInfo) => void
}

/** 默认值与后端一致：100 GB / 上下行 100 Mbps / 30 天。 */
const DEFAULTS = { quotaGb: '100', up: '100', down: '100', days: '30' }

function validNumber(raw: string, min: number, max: number): boolean {
  const n = Number(raw)
  return Number.isFinite(n) && n >= min && n <= max
}

export default function ShareWizard({ servers, onClose, onCreated }: Props) {
  const [serverId, setServerId] = useState<number | null>(servers[0]?.id ?? null)
  const [candidates, setCandidates] = useState<ShareCandidate[]>([])
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const [tls, setTls] = useState<ShareTlsStatus | null>(null)
  const [loading, setLoading] = useState(false)
  const [tlsBusy, setTlsBusy] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [quotaGb, setQuotaGb] = useState(DEFAULTS.quotaGb)
  const [rateUp, setRateUp] = useState(DEFAULTS.up)
  const [rateDown, setRateDown] = useState(DEFAULTS.down)
  const [days, setDays] = useState(DEFAULTS.days)

  // 服务器列表可能晚于向导加载完成；首个服务器到达后自动选中。
  useEffect(() => {
    if (serverId == null && servers[0]) setServerId(servers[0].id)
  }, [serverId, servers])

  useEffect(() => {
    if (serverId == null) return
    let alive = true
    setLoading(true)
    setError(null)
    setTls(null)
    Promise.all([listShareCandidates(serverId), shareTlsStatus(serverId)])
      .then(([list, status]) => {
        if (!alive) return
        setCandidates(list)
        // 默认勾选全部可分享节点；不可分享的保持未选。
        setPicked(new Set(list.filter((c) => c.shareable).map((c) => c.node.id)))
        setTls(status)
      })
      .catch((e) => alive && setError(String(e)))
      .finally(() => alive && setLoading(false))
    return () => {
      alive = false
    }
  }, [serverId])

  const toggle = (id: string) => {
    setPicked((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const handlePrepareTls = async () => {
    if (serverId == null) return
    setError(null)
    setTlsBusy(true)
    try {
      setTls(await prepareShareTls(serverId))
    } catch (e) {
      setError(String(e))
    } finally {
      setTlsBusy(false)
    }
  }

  const optionsValid =
    validNumber(quotaGb, 1, Number.MAX_SAFE_INTEGER) &&
    validNumber(rateUp, 0, 100_000) &&
    validNumber(rateDown, 0, 100_000) &&
    validNumber(days, 1, 3650)
  const canCreate =
    serverId != null && picked.size > 0 && tls?.ready === true && optionsValid && !busy

  const handleCreate = async () => {
    if (!canCreate || serverId == null) return
    setError(null)
    setBusy(true)
    try {
      const info = await createShare(
        serverId,
        [...picked],
        Number(quotaGb),
        Number(rateUp),
        Number(rateDown),
        Number(days),
      )
      onCreated(info)
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-3 rounded border border-neutral-700 bg-neutral-900 p-3 text-sm">
      <div className="flex items-center justify-between">
        <h3 className="font-medium">新建分享</h3>
        <button
          type="button"
          onClick={onClose}
          disabled={busy || tlsBusy}
          className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700 disabled:opacity-50"
        >
          关闭
        </button>
      </div>

      {error && (
        <p role="alert" className="rounded bg-red-950 px-2 py-1 text-xs text-red-300">
          {error}
        </p>
      )}

      <label className="flex items-center gap-2">
        服务器
        <select
          aria-label="分享服务器"
          value={serverId ?? ''}
          onChange={(e) => setServerId(Number(e.target.value))}
          className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
        >
          {servers.map((s) => (
            <option key={s.id} value={s.id}>
              {s.name}（{s.host}）
            </option>
          ))}
        </select>
      </label>

      <div className="rounded bg-neutral-950 px-2 py-1.5 text-xs">
        {tls === null ? (
          <span className="text-neutral-400">读取 HTTPS 状态…</span>
        ) : tls.ready ? (
          <span className="text-emerald-400">
            HTTPS 已就绪：{tls.domain}
            {tls.cert_expires_at ? `（证书到期 ${tls.cert_expires_at.slice(0, 10)}）` : ''}
          </span>
        ) : (
          <span className="flex flex-wrap items-center gap-2 text-amber-300">
            不能生成：{tls.detail}
            <button
              type="button"
              onClick={() => void handlePrepareTls()}
              disabled={tlsBusy || serverId == null}
              className="rounded bg-amber-700 px-2 py-1 text-xs text-white hover:bg-amber-600 disabled:opacity-50"
            >
              {tlsBusy ? '申请证书中…' : '配置 HTTPS'}
            </button>
          </span>
        )}
      </div>

      {loading ? (
        <p className="text-xs text-neutral-400">读取候选中…</p>
      ) : (
        <ul className="max-h-64 space-y-1 overflow-y-auto">
          {candidates.map((c) => (
            <li key={c.node.id} className="flex items-center gap-2">
              <input
                type="checkbox"
                aria-label={`选择 ${c.node.name}`}
                checked={picked.has(c.node.id)}
                disabled={!c.shareable || busy}
                onChange={() => toggle(c.node.id)}
              />
              <span>{c.node.name}</span>
              <span className="text-xs text-neutral-500">{c.node.protocol}</span>
              {c.reason && <span className="text-xs text-amber-500">{c.reason}</span>}
            </li>
          ))}
        </ul>
      )}

      <div className="grid grid-cols-2 gap-2">
        <label className="flex flex-col gap-1 text-xs text-neutral-400">
          配额（GB）
          <input
            aria-label="配额（GB）"
            type="number"
            min={1}
            value={quotaGb}
            onChange={(e) => setQuotaGb(e.target.value)}
            className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm text-neutral-100"
          />
        </label>
        <label className="flex flex-col gap-1 text-xs text-neutral-400">
          有效期（天）
          <input
            aria-label="有效期（天）"
            type="number"
            min={1}
            max={3650}
            value={days}
            onChange={(e) => setDays(e.target.value)}
            className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm text-neutral-100"
          />
        </label>
        <label className="flex flex-col gap-1 text-xs text-neutral-400">
          上行 Mbps（0 = 不限）
          <input
            aria-label="上行 Mbps"
            type="number"
            min={0}
            max={100000}
            value={rateUp}
            onChange={(e) => setRateUp(e.target.value)}
            className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm text-neutral-100"
          />
        </label>
        <label className="flex flex-col gap-1 text-xs text-neutral-400">
          下行 Mbps（0 = 不限）
          <input
            aria-label="下行 Mbps"
            type="number"
            min={0}
            max={100000}
            value={rateDown}
            onChange={(e) => setRateDown(e.target.value)}
            className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm text-neutral-100"
          />
        </label>
      </div>

      <div className="flex gap-2">
        <button
          type="button"
          onClick={() => void handleCreate()}
          disabled={!canCreate}
          className="rounded bg-blue-600 px-3 py-1.5 text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? '生成中…' : `生成链接（${picked.size} 条）`}
        </button>
        <button
          type="button"
          onClick={onClose}
          disabled={busy || tlsBusy}
          className="rounded bg-neutral-800 px-3 py-1.5 hover:bg-neutral-700 disabled:opacity-50"
        >
          取消
        </button>
      </div>
    </div>
  )
}
