// 节点列表的一行：勾选、就地改名、延迟/带宽/上次测速、失败原因、启用、隐私保护、
// 中转开关与操作（测速/删除）。
import type { Node, Privacy } from '../../api/types'
import type { TargetResult } from '../../jobs/registry'
import NodeLatencyCell from '../../components/NodeLatencyCell'
import NodePrivacyCell from '../../components/NodePrivacyCell'
import { formatCheckedAt } from '../../utils/checkedAtFormat'
import { formatBandwidth } from '../../utils/format'

export default function NodeRow({
  node: n,
  selected,
  onToggleSelect,
  editingDraft,
  onEditingDraftChange,
  onRenameSave,
  onRenameCancel,
  savingName,
  live,
  privacy,
  privacyBusy,
  onTogglePrivacy,
  showRelayCol,
  needsCompletion,
  completionTitle,
  onCompleteRelay,
  relayOn,
  onToggleRelay,
  speedBusy,
  onSpeedTest,
  onRenameStart,
  onDelete,
  onToggleEnabled,
}: {
  node: Node
  selected: boolean
  onToggleSelect: () => void
  /** 非 null = 这一行正在就地改名 */
  editingDraft: string | null
  onEditingDraftChange: (draft: string) => void
  onRenameSave: () => void
  onRenameCancel: () => void
  savingName: boolean
  live: TargetResult | undefined
  privacy: Privacy | undefined
  privacyBusy: boolean
  onTogglePrivacy: () => void
  showRelayCol: boolean
  needsCompletion: boolean
  completionTitle: string
  onCompleteRelay: () => void
  relayOn: boolean
  onToggleRelay: () => void
  speedBusy: boolean
  onSpeedTest: () => void
  onRenameStart: () => void
  onDelete: () => void
  onToggleEnabled: () => void
}) {
  return (
    <tr className="border-b border-neutral-900">
      <td className="px-3 py-2">
        <input
          type="checkbox"
          aria-label={`勾选 ${n.name}`}
          checked={selected}
          onChange={onToggleSelect}
          className="accent-indigo-600"
        />
      </td>
      <td className="px-3 py-2">
        {editingDraft != null ? (
          <input
            autoFocus
            aria-label={`新名称 ${n.name}`}
            value={editingDraft}
            onChange={(e) => onEditingDraftChange(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') onRenameSave()
              if (e.key === 'Escape') onRenameCancel()
            }}
            className="w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm"
          />
        ) : (
          n.name
        )}
      </td>
      <td className="px-3 py-2 text-neutral-400">{n.protocol}</td>
      <td className="px-3 py-2 text-neutral-400">
        {n.addr}:{n.port}
      </td>
      <td className="px-3 py-2 text-neutral-400">{n.group_name}</td>
      <NodeLatencyCell node={n} />
      <td className="px-3 py-2 text-neutral-400">
        {n.bandwidth != null ? formatBandwidth(n.bandwidth) : '—'}
      </td>
      <td className="px-3 py-2 text-neutral-400">{formatCheckedAt(n.last_checked_at)}</td>
      <td className="px-3 py-2 text-sm">
        {live && !live.error ? (
          <span className="text-emerald-400">已完成</span>
        ) : (live?.error ?? n.last_check_error) ? (
          <span className="text-red-400">{live?.error ?? n.last_check_error}</span>
        ) : (
          <span className="text-neutral-500">—</span>
        )}
      </td>
      <td className="px-3 py-2">
        <button
          type="button"
          role="switch"
          aria-checked={n.enabled}
          onClick={onToggleEnabled}
          className={`relative h-5 w-9 shrink-0 align-middle rounded-full transition-colors ${
            n.enabled ? 'bg-emerald-600' : 'bg-neutral-700'
          }`}
        >
          <span
            className={`absolute left-0.5 top-0.5 h-4 w-4 rounded-full bg-white transition-transform ${
              n.enabled ? 'translate-x-4' : 'translate-x-0'
            }`}
          />
        </button>
      </td>
      <td className="px-3 py-2">
        <NodePrivacyCell privacy={privacy} busy={privacyBusy} onToggle={onTogglePrivacy} />
      </td>
      {showRelayCol && (
        <td className="px-3 py-2">
          {needsCompletion ? (
            <button
              type="button"
              onClick={onCompleteRelay}
              title={completionTitle}
              className="rounded bg-amber-900 px-2 py-1 text-xs text-amber-200 hover:bg-amber-800"
            >
              补全中转
            </button>
          ) : (
            <button
              type="button"
              onClick={onToggleRelay}
              disabled={speedBusy}
              title={relayOn ? '经中转机转发（关=直连）' : '直连（开=经中转机转发）'}
              className={`rounded px-2 py-1 text-xs hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50 ${
                relayOn ? 'bg-sky-900 text-sky-300' : 'bg-neutral-800 text-neutral-400'
              }`}
            >
              {relayOn ? '中转✓' : '中转'}
            </button>
          )}
        </td>
      )}
      <td className="px-3 py-2">
        <div className="flex items-center gap-2">
          {editingDraft != null ? (
            <>
              <button
                type="button"
                onClick={onRenameSave}
                disabled={savingName || editingDraft.trim() === ''}
                className="rounded bg-blue-600 px-2 py-1 text-xs font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {savingName ? '保存中…' : '保存'}
              </button>
              <button
                type="button"
                onClick={onRenameCancel}
                disabled={savingName}
                className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
              >
                取消
              </button>
            </>
          ) : (
            <button
              type="button"
              onClick={onRenameStart}
              className="rounded bg-neutral-800 px-2 py-1 text-xs text-amber-300 hover:bg-neutral-700"
            >
              改名
            </button>
          )}
          <button
            type="button"
            onClick={onSpeedTest}
            disabled={speedBusy}
            className="rounded bg-neutral-800 px-2 py-1 text-xs text-sky-300 hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {speedBusy ? '测速中…' : '测速'}
          </button>
          <button
            type="button"
            onClick={onDelete}
            className="rounded bg-neutral-800 px-2 py-1 text-xs text-red-300 hover:bg-neutral-700"
          >
            删除
          </button>
        </div>
      </td>
    </tr>
  )
}
