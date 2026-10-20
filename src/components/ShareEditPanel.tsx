// 编辑分享：配额/上下行速率/剩余有效期；重置用量独立二次确认。
// 编辑会停-写-起网关并保留已用字节；到期时间按「从现在起 N 天」重新计算。
import { useState } from 'react'
import { resetShareUsage, updateShare } from '../api'
import ConfirmButton from './ConfirmButton'
import type { ShareInfo } from '../api/types'

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

  const optionsValid =
    validNumber(quotaUpGb, 0, Number.MAX_SAFE_INTEGER) &&
    validNumber(quotaDownGb, 0, Number.MAX_SAFE_INTEGER) &&
    (Number(quotaUpGb) > 0 || Number(quotaDownGb) > 0) &&
    validNumber(rateUp, 0, 100_000) &&
    validNumber(rateDown, 0, 100_000) &&
    validNumber(days, 1, 3650)

  const handleSave = async () => {
    if (!optionsValid) return
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
          disabled={busy || !optionsValid}
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
