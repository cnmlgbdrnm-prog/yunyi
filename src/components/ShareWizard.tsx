// 新建分享向导：服务器 → 准入候选 → HTTPS 状态 → 配额/速率/到期 → 创建。
// 准入以后端 `listShareCandidates` 为准；前端只负责置灰与展示原因，不自行判断。
import { useEffect, useState } from 'react'
import { getSettings, listShareCandidates, prepareShareTls, shareTlsStatus } from '../api'
import type { Server, ShareCandidate, ShareInfo, ShareTlsStatus } from '../api/types'
import { runShareCreation, useShareCreation } from '../views/shareCreationStore'
import { applyShareTls } from '../views/shareStatusStore'

interface Props {
  servers: Server[]
  onClose: () => void
  onCreated: (info: ShareInfo) => void
}

/** 默认值与后端一致：总额度 100 GB / 上下行 100 Mbps / 30 天。
 *  上下行只记录用量，不再作为独立配额。 */
const DEFAULTS = { up: '100', down: '100', days: '30' }

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
  // 忙碌态来自模块级 store：切页卸载再挂回来仍能拦住重复提交（原先存本地 state，
  // 切走再打开向导按钮又变回可点）。
  const creation = useShareCreation()
  const busy = creation?.status === 'running'
  const [error, setError] = useState<string | null>(null)
  const [quotaTotalGb, setQuotaTotalGb] = useState('100')
  const [rateUp, setRateUp] = useState(DEFAULTS.up)
  const [rateDown, setRateDown] = useState(DEFAULTS.down)
  const [days, setDays] = useState(DEFAULTS.days)
  const [cdnEnabled, setCdnEnabled] = useState(false)
  const [cdnDomain, setCdnDomain] = useState('')
  const [cdnPort, setCdnPort] = useState('0')

  const cdnCompatible = (c: ShareCandidate) => {
    try {
      const p = JSON.parse(c.node.params_json)
      return ['vmess', 'vless'].includes(c.node.protocol) && (p.network ?? p.type) === 'ws' && p.security !== 'reality' && !p['reality-opts']
    } catch { return false }
  }


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
    setCdnEnabled(false)
    setCdnDomain('')
    setCdnPort('0')
    const key = `share_cdn_${serverId}`
    Promise.all([listShareCandidates(serverId), shareTlsStatus(serverId), getSettings([key])])
      .then(([list, status, settings]) => {
        if (!alive) return
        setCandidates(list)
        // 默认勾选全部可分享节点；不可分享的保持未选。
        const cdn = settings[key] ? JSON.parse(settings[key]) : null
        setCdnEnabled(Boolean(cdn))
        setCdnDomain(cdn?.domain ?? '')
        setCdnPort(String(cdn?.port ?? 0))
        setPicked(new Set(list.filter((c) => c.shareable && (!cdn || cdnCompatible(c))).map((c) => c.node.id)))
        setTls(status)
        // 后端缓存优先返回；把结果同步进 store，分享页下次切回无需再查。
        applyShareTls(serverId, status)
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
      const status = await prepareShareTls(serverId)
      setTls(status)
      applyShareTls(serverId, status)
    } catch (e) {
      setError(String(e))
    } finally {
      setTlsBusy(false)
    }
  }

  const optionsValid =
    validNumber(quotaTotalGb, 0, Number.MAX_SAFE_INTEGER) &&
    validNumber(rateUp, 0, 100_000) &&
    validNumber(rateDown, 0, 100_000) &&
    validNumber(days, 1, 3650)
  const canCreate =
    serverId != null && picked.size > 0 && !loading &&
    (cdnEnabled ? /^[a-zA-Z0-9.-]+$/.test(cdnDomain.trim()) && cdnDomain.includes('.') : tls?.ready === true) &&
    optionsValid && !busy

  const handleCreate = async () => {
    if (!canCreate || serverId == null) return
    setError(null)
    try {
      const info = await runShareCreation({
        serverId,
        serverName: servers.find((s) => s.id === serverId)?.name ?? String(serverId),
        nodeIds: [...picked],
        quotaTotalGb: Number(quotaTotalGb),
        rateUpMbps: Number(rateUp),
        rateDownMbps: Number(rateDown),
        expiresInDays: Number(days),
        cdn: cdnEnabled ? { domain: cdnDomain.trim(), port: Number(cdnPort) } : null,
      })
      onCreated(info)
    } catch (e) {
      setError(String(e))
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
          disabled={busy}
          className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 disabled:opacity-50"
        >
          {servers.map((s) => (
            <option key={s.id} value={s.id}>
              {s.name}（{s.host}）
            </option>
          ))}
        </select>
      </label>

      <fieldset disabled={busy || loading} className="space-y-2 rounded border border-neutral-700 p-2">
        <label className="flex items-center gap-2">
          <input type="checkbox" aria-label="通过 CDN 分享" checked={cdnEnabled} onChange={(e) => {
            setCdnEnabled(e.target.checked)
            setPicked(new Set(candidates.filter((c) => c.shareable && (!e.target.checked || cdnCompatible(c))).map((c) => c.node.id)))
          }} />
          通过 CDN 分享
        </label>
        {cdnEnabled && <>
          <label className="flex items-center gap-2">CDN 域名
            <input aria-label="CDN 域名" value={cdnDomain} onChange={(e) => setCdnDomain(e.target.value)} placeholder="us-cdn.example.com" className="rounded bg-neutral-800 px-2 py-1" />
          </label>
          <label className="flex items-center gap-2">CDN HTTPS 端口
            <select aria-label="CDN HTTPS 端口" value={cdnPort} onChange={(e) => setCdnPort(e.target.value)} className="rounded bg-neutral-800 px-2 py-1">
              <option value="0">自动选择空闲端口</option>
              {[443, 2053, 2083, 2087, 2096, 8443].map((port) => <option key={port} value={port}>{port}</option>)}
            </select>
          </label>
          <p className="text-xs text-neutral-400">域名须已开启 CDN 代理，源站须已有该域名的有效 Let's Encrypt 证书。支持 VMess/VLESS WebSocket，保留分享的流量与到期限制。生成时记住该服务器配置；已有分享保持原入口。</p>
        </>}
      </fieldset>

      <div className="rounded bg-neutral-950 px-2 py-1.5 text-xs">
        {cdnEnabled ? (
          <span className="text-emerald-400">CDN 分享：订阅与 WS 节点共用 HTTPS 入口，创建时验证证书与回源。</span>
        ) : tls === null ? (
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
                disabled={!c.shareable || busy || (cdnEnabled && !cdnCompatible(c))}
                onChange={() => toggle(c.node.id)}
              />
              <span>{c.node.name}</span>
              <span className="text-xs text-neutral-500">{c.node.protocol}</span>
              {cdnEnabled && !cdnCompatible(c) && <span className="text-xs text-amber-500">该协议不能走 CDN</span>}
              {c.reason && <span className="text-xs text-amber-500">{c.reason}</span>}
            </li>
          ))}
        </ul>
      )}

      <p className="text-xs text-neutral-500">
        1 GB = 10 亿字节。只按公网双向总额度停用；上行、下行只记录用量，不再单独限额。
      </p>
      <div className="grid grid-cols-2 gap-2">
        <label className="col-span-2 flex flex-col gap-1 text-xs text-neutral-400">
          公网双向总额度（GB，0 = 不限）
          <input aria-label="总额度（GB）" type="number" min={0} value={quotaTotalGb}
            onChange={(e) => setQuotaTotalGb(e.target.value)}
            className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm text-neutral-100" />
          <span>公网入站、出站共同扣减总额度；不重复计算服务器内部回环流量。</span>
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
