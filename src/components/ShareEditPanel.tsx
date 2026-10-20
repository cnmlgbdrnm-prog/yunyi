// 编辑分享：节点增删、配额/上下行速率/剩余有效期；重置用量独立二次确认。
// 编辑会停-写-起网关并保留已用字节；到期时间按「从现在起 N 天」重新计算。
// 节点只在选择集合发生变化时才提交：仍被选中的节点沿用原端口与凭据，链接不变。
import { useEffect, useState } from 'react'
import { listShareCandidates, resetShareUsage, updateShare } from '../api'
import ConfirmButton from './ConfirmButton'
import type { ShareCandidate, ShareInfo } from '../api/types'

interface Props {
  share: ShareInfo
  onClose: () => void
  onSaved: (info: ShareInfo) => void
}

const GB = 1024 ** 3

function remainingDays(expiresAt: string | null): string {
  if (!expiresAt) return '30'
  const ms = new Date(expiresAt).getTime() - Date.now()
  return String(Math.max(1, Math.ceil(ms / 86_400_000)))
}

function validNumber(raw: string, min: number, max: number): boolean {
  const n = Number(raw)
  return Number.isFinite(n) && n >= min && n <= max
}

export default function ShareEditPanel({ share, onClose, onSaved }: Props) {
  // 旧分享只有合计配额：首次编辑时把它当作下行配额，上行默认不限（0）。
  // 这与网关的迁移规则一致——历史合计全部计入下行，避免凭空多出额度。
  const [quotaUpGb, setQuotaUpGb] = useState(
    share.split_quota ? String(Math.round(share.quota_up_bytes / GB)) : '0',
  )
  const [quotaDownGb, setQuotaDownGb] = useState(
    String(
      share.split_quota
        ? Math.round(share.quota_down_bytes / GB)
        : Math.max(1, Math.round(share.quota_bytes / GB)),
    ),
  )
  const [rateUp, setRateUp] = useState(String(share.rate_up_mbps))
  const [rateDown, setRateDown] = useState(String(share.rate_down_mbps))
  const [days, setDays] = useState(remainingDays(share.expires_at))
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [candidates, setCandidates] = useState<ShareCandidate[]>([])
  const [initialNodeIds, setInitialNodeIds] = useState<Set<string> | null>(null)
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const [nodesReady, setNodesReady] = useState(false)
  const [nodesLoading, setNodesLoading] = useState(true)
  const [nodesError, setNodesError] = useState<string | null>(null)

  // 节点候选来自后端；当前分享的节点按名字映射回候选 id。读不到候选时
  // 仍然允许只改配额——此时不传 nodeIds，后端不会动节点。
  useEffect(() => {
    let alive = true
    setNodesLoading(true)
    setNodesError(null)
    setNodesReady(false)
    listShareCandidates(share.server_id)
      .then((list) => {
        if (!alive) return
        const currentNames = new Set(share.node_names)
        const current = new Set(
          list
            .filter((c) => currentNames.has(c.node.name))
            .map((c) => c.node.id),
        )
        setCandidates(list)
        setInitialNodeIds(current)
        setPicked(new Set(current))
        setNodesReady(true)
      })
      .catch((e) => {
        if (!alive) return
        setNodesError(`节点列表读取失败：${String(e)}；保存只改配额，不会改动节点`)
      })
      .finally(() => {
        if (alive) setNodesLoading(false)
      })
    return () => {
      alive = false
    }
  }, [share.id, share.server_id, share.node_names])

  const toggleNode = (id: string) => {
    setPicked((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const ssPicked = candidates.filter(
    (c) => picked.has(c.node.id) && c.node.protocol === 'ss',
  ).length
  const unmatchedCurrent = share.node_names.filter(
    (name) => !candidates.some((c) => c.node.name === name),
  )
  const nodeSetChanged =
    nodesReady &&
    initialNodeIds !== null &&
    (picked.size !== initialNodeIds.size ||
      [...picked].some((id) => !initialNodeIds.has(id)))
  // 只有用户真的改了节点集合才校验「至少一个」「SS 最多一个」；否则允许只改配额，
  // 也避免当前节点不在候选列表时把人锁死。
  const nodeSetValid =
    !nodesReady || !nodeSetChanged || (picked.size > 0 && ssPicked <= 1)

  const optionsValid =
    validNumber(quotaUpGb, 0, Number.MAX_SAFE_INTEGER) &&
    validNumber(quotaDownGb, 0, Number.MAX_SAFE_INTEGER) &&
    (Number(quotaUpGb) > 0 || Number(quotaDownGb) > 0) &&
    validNumber(rateUp, 0, 100_000) &&
    validNumber(rateDown, 0, 100_000) &&
    validNumber(days, 1, 3650)
  const canSave = optionsValid && nodeSetValid && !nodesLoading && !busy

  const handleSave = async () => {
    if (!canSave) return
    setError(null)
    setBusy(true)
    try {
      onSaved(
        await updateShare(
          share.server_id,
          share.id,
          Number(quotaUpGb),
          Number(quotaDownGb),
          Number(rateUp),
          Number(rateDown),
          Number(days),
          nodeSetChanged ? [...picked] : undefined,
        ),
      )
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  const handleReset = async () => {
    setError(null)
    setBusy(true)
    try {
      onSaved(await resetShareUsage(share.server_id, share.id))
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-3 rounded border border-neutral-700 bg-neutral-900 p-3 text-sm">
      <div className="flex items-center justify-between">
        <h3 className="font-medium">编辑分享：{share.node_names.join('、')}</h3>
        <button
          type="button"
          onClick={onClose}
          disabled={busy}
          className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700 disabled:opacity-50"
        >
          关闭
        </button>
      </div>

      {share.status === 'blocked_expired' && (
        <p className="rounded bg-amber-950 px-2 py-1 text-xs text-amber-300">
          已到期：保存时会按下面的「有效期」从现在重新计算到期时间。
        </p>
      )}
      {!share.split_quota && (
        <p className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-400">
          旧口径：当前只有一个合计配额。保存后改为上下行分开配额，历史合计用量计入下行，上行默认不限。
        </p>
      )}
      {error && (
        <p role="alert" className="rounded bg-red-950 px-2 py-1 text-xs text-red-300">
          {error}
        </p>
      )}

      <div className="space-y-1">
        <div className="text-xs text-neutral-400">
          节点（取消勾选即删除，勾选新节点即添加；保留的节点链接与凭据不变）
        </div>
        {nodesLoading ? (
          <p className="text-xs text-neutral-400">读取候选节点…</p>
        ) : nodesReady ? (
          <>
            <ul className="max-h-48 space-y-1 overflow-y-auto rounded bg-neutral-950 p-2">
              {candidates.map((c) => {
                const checked = picked.has(c.node.id)
                return (
                  <li key={c.node.id}>
                    <label className="flex items-center gap-2">
                      <input
                        type="checkbox"
                        aria-label={`选择节点 ${c.node.name}`}
                        checked={checked}
                        disabled={busy || (!c.shareable && !checked)}
                        onChange={() => toggleNode(c.node.id)}
                      />
                      <span>{c.node.name}</span>
                      <span className="text-xs text-neutral-500">{c.node.protocol}</span>
                      {c.reason && (
                        <span className="text-xs text-amber-500">
                          {c.reason}
                          {checked ? '（保留现有节点）' : ''}
                        </span>
                      )}
                    </label>
                  </li>
                )
              })}
            </ul>
            {nodeSetChanged && picked.size === 0 && (
              <p className="text-xs text-red-300">至少保留一个节点</p>
            )}
            {nodeSetChanged && ssPicked > 1 && (
              <p className="text-xs text-red-300">
                Shadowsocks 一次只能保留一个节点
              </p>
            )}
            {unmatchedCurrent.length > 0 && (
              <p className="rounded bg-amber-950 px-2 py-1 text-xs text-amber-300">
                当前分享中的节点「{unmatchedCurrent.join('、')}」不在候选列表（可能已停用或删除）；
                改动节点选择并保存后会被移除。
              </p>
            )}
          </>
        ) : (
          <p role="status" className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-400">
            {nodesError}
          </p>
        )}
      </div>

      <div className="grid grid-cols-2 gap-2">
        <label className="flex flex-col gap-1 text-xs text-neutral-400">
          上行配额（GB，0 = 不限）
          <input
            aria-label="上行配额（GB）"
            type="number"
            min={0}
            value={quotaUpGb}
            onChange={(e) => setQuotaUpGb(e.target.value)}
            className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm text-neutral-100"
          />
        </label>
        <label className="flex flex-col gap-1 text-xs text-neutral-400">
          下行配额（GB，0 = 不限）
          <input
            aria-label="下行配额（GB）"
            type="number"
            min={0}
            value={quotaDownGb}
            onChange={(e) => setQuotaDownGb(e.target.value)}
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

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={() => void handleSave()}
          disabled={!canSave}
          className="rounded bg-blue-600 px-3 py-1.5 text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? '保存中…' : '保存'}
        </button>
        <ConfirmButton
          label="重置用量"
          confirmLabel="确认重置"
          busyLabel="重置中…"
          busy={busy}
          onConfirm={() => void handleReset()}
        />
        <button
          type="button"
          onClick={onClose}
          disabled={busy}
          className="rounded bg-neutral-800 px-3 py-1.5 hover:bg-neutral-700 disabled:opacity-50"
        >
          取消
        </button>
      </div>
      <p className="text-xs text-neutral-500">
        重置只清空已用流量并解除停用，不改配额与到期时间；已导入的客户端仍使用原链接。
      </p>
    </div>
  )
}
