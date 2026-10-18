// 链路列表行：名称/两端服务器/模式与端口 + 状态徽标与握手/传输（数据派生，
// 缺失兜底「状态未知」）+ 刷新/拆链/删除（二次确认）/设备展开 + 拆链任务进度。
// 事件回调由父级注入，本组件只做展示与本地确认交互。
import type { LinkStatus, RelayRule, Server } from '../api/types'
import type { ProgressView } from './JobProgressPanel'
import { formatBytes, formatHandshakeAgo, handshakeSecs } from '../utils/format'
import ConfirmButton from './ConfirmButton'
import DevicePeersPanel from './DevicePeersPanel'
import StatusBadge from './StatusBadge'

const MODE_LABEL: Record<string, string> = { forward: '模式 A', peer: '模式 B' }

interface Props {
  rule: RelayRule
  relay: Server | undefined
  landing: Server | undefined
  status: LinkStatus | undefined
  refreshing: boolean
  teardownActive: boolean
  teardown: ProgressView | null
  deleting: boolean
  readopting: boolean
  expanded: boolean
  rowSuccess: string | undefined
  rowError: string | undefined
  onRefresh: () => void
  onReadopt: () => void
  onTeardown: (action: 'unregister' | 'full') => void
  onCancelTeardown: () => void
  onDelete: () => void
  onToggleDevices: () => void
}

export default function LinkRow({
  rule,
  relay,
  landing,
  status,
  refreshing,
  teardownActive,
  teardown,
  deleting,
  readopting,
  expanded,
  rowSuccess,
  rowError,
  onRefresh,
  onReadopt,
  onTeardown,
  onCancelTeardown,
  onDelete,
  onToggleDevices,
}: Props) {
  const showStatus = rule.status === 'built' || rule.status === 'imported'
  const peersUsable = rule.client_mode === 'peer' && (rule.status === 'built' || rule.status === 'imported')
  return (
    <li className="rounded border border-neutral-800 p-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 space-y-1">
          <p className="font-medium">{rule.name}</p>
          <p className="text-xs text-neutral-500">
            {relay ? `${relay.name}（${relay.host}）` : `中转 #${rule.relay_server_id}`} →{' '}
            {landing ? `${landing.name}（${landing.host}）` : `落地 #${rule.landing_server_id}`}
          </p>
          <p className="text-xs text-neutral-500">
            {MODE_LABEL[rule.client_mode] ?? rule.client_mode} · {rule.client_mode === 'forward' ? `转发端口 ${rule.forward_port ?? '-'}` : `WG 端口 ${rule.wg_port}`} ·
            落地端口 {rule.landing_port} · 网段 {rule.subnet}
          </p>
          {/* 状态行只在 built/imported 上显示（`showStatus`）——三个分支**共用同一道门**。
              原先只有兜底那支带门，另两支没有：于是拆链后残留的状态值（或切页重挂时从
              模块级缓存播种回来的旧握手）会挂在一条 not-built 的链路上，看着像它还活着。
              而且 `refresh_link_status` 不看规则状态（relay/mod.rs:843 直接按公钥查两端），
              对 not-built 的行它报的是残留接口的数字，显示出来是误导而不是信息。 */}
          {showStatus && status != null && status.ok ? (
            <>
              <p className="text-xs text-neutral-400">最近握手：{formatHandshakeAgo(handshakeSecs(status))}</p>
              <p className="text-xs text-neutral-400">
                传输 收 {formatBytes(status.relay_rx_bytes + status.landing_rx_bytes)} / 发 {formatBytes(status.relay_tx_bytes + status.landing_tx_bytes)}
              </p>
            </>
          ) : showStatus && status != null ? (
            <p className="text-xs text-red-400">状态未知：{status.error}</p>
          ) : showStatus && !refreshing ? (
            <p className="text-xs text-neutral-500">状态未知</p>
          ) : null}
          {rowSuccess && <p className="text-xs text-emerald-400">{rowSuccess}</p>}
          {rowError && <p className="text-xs text-red-400">{rowError}</p>}
        </div>
        <div className="flex shrink-0 flex-col items-end gap-2">
          <StatusBadge status={rule.status} />
          <div className="flex flex-wrap justify-end gap-2">
            {/* 「刷新状态」与上面的状态行同进同退：查出来的结果既然只对 built/imported
                显示，那对别的行就不该摆一个按了没反应的按钮。同页「设备」按钮已按
                `peersUsable` 这么设门（LinkRow.tsx:43），这里只是把同一条规矩补齐。 */}
            {showStatus && (
              <button
                type="button"
                disabled={refreshing}
                onClick={onRefresh}
                className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700 disabled:opacity-50"
              >
                {refreshing ? '查询中…' : '刷新状态'}
              </button>
            )}
            {peersUsable && (
              <button type="button" onClick={onToggleDevices} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
                {expanded ? '收起设备' : '设备'}
              </button>
            )}
            {rule.status === 'failed' && (
              <button
                type="button"
                disabled={readopting}
                onClick={onReadopt}
                className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50"
              >
                {readopting ? '接管中…' : '重新接管'}
              </button>
            )}
            {rule.status !== 'not-built' && rule.status !== 'failed' ? (
              rule.status === 'imported' ? (
                <>
                  <ConfirmButton
                    label="解除登记"
                    confirmLabel={`确认解除登记 ${rule.name}（不动服务器配置）`}
                    busyLabel="解除中…"
                    busy={teardownActive}
                    onConfirm={() => onTeardown('unregister')}
                  />
                  <ConfirmButton
                    label="拆除接口"
                    confirmLabel={`确认拆除接口 ${rule.name}（删除两台服务器上的 WireGuard 配置）`}
                    busyLabel="拆除中…"
                    busy={teardownActive}
                    onConfirm={() => onTeardown('full')}
                  />
                </>
              ) : (
                <ConfirmButton
                  label="拆链"
                  confirmLabel={`确认拆链 ${rule.name}`}
                  busyLabel="拆链中…"
                  busy={teardownActive}
                  onConfirm={() => onTeardown('full')}
                />
              )
            ) : (
              <ConfirmButton
                label="删除"
                confirmLabel={
                  rule.status === 'failed'
                    ? `确认删除 ${rule.name}（不清理服务器残留配置）`
                    : `确认删除 ${rule.name}`
                }
                busyLabel="删除中…"
                busy={deleting}
                onConfirm={onDelete}
              />
            )}
          </div>
        </div>
      </div>
      {teardownActive && (
        <div className="mt-2 space-y-1 rounded border border-neutral-800 p-2 text-sm text-neutral-400">
          {teardown ? (
            <>
              <p>
                步骤：{teardown.step}
                {teardown.message !== '' && `（${teardown.message}）`}
              </p>
              <div className="h-2 w-full overflow-hidden rounded bg-neutral-800">
                <div className="h-full bg-emerald-600 transition-all" style={{ width: `${Math.min(100, Math.max(0, teardown.progress))}%` }} />
              </div>
            </>
          ) : (
            <p>拆链任务已提交，等待执行…</p>
          )}
          <button type="button" onClick={onCancelTeardown} className="rounded bg-red-800 px-2 py-1 text-xs text-white hover:bg-red-700">
            取消任务
          </button>
        </div>
      )}
      {expanded && (
        <div className="mt-2">
          <DevicePeersPanel ruleId={rule.id} />
        </div>
      )}
    </li>
  )
}
