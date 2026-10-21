// 服务器列表里的一张卡片：身份/地址/测速结果 + 主操作 + 「更多」副操作 + 展开区。
//
// 数据聚合全留在父级（Servers）：这台服务器的每一项状态都来自不同的 state（测速任务表、
// BBR 三态、部署摘要、连通测试、通道/上游展开），在本组件里再拼一遍等于把父级的
// 状态结构复制一份。所以这里只接**已经取好的那一条**，外加回调。
//
// 展开区（部署向导 / 编辑表单）由父级作为 children 注入：那两块各自持有自己的状态与副作用，
// 放进来只会让本组件重新变成一个什么都管的巨型组件。
import type { ReactNode } from 'react'
import type { ConnectionTest, DeployedProtocolDetail, Server } from '../api/types'
import ConfirmButton from './ConfirmButton'
import ChannelPanel from './ChannelPanel'
import DeployedProtocolList from './DeployedProtocolList'
import ExpandableCard from './ExpandableCard'
import UpstreamPanel from './UpstreamPanel'
import { effectiveServerHost, formatHostPort } from '../utils/addrFamily'
import { formatCheckedAt } from '../utils/checkedAtFormat'
import { formatBandwidth } from '../utils/format'
import { serverKindLabel } from '../utils/serverKindLabel'

interface Props {
  server: Server
  /** 行内测速状态：busy=有在跟踪的任务覆盖这台；error=该行失败原因；done=有结果且无错 */
  speed: { busy: boolean; error: string | null; done: boolean }
  testing: boolean
  /** 地址族切换进行中（服务器 + 全部自建直连节点一起改，期间锁住开关） */
  addrBusy: boolean
  /** 切到 v6 时若未登记地址，后端会自动 SSH 探测并登记后再切 */
  onAddrFamily: (family: 'v4' | 'v6') => void
  /** 一键免密进行中 */
  passwordlessBusy: boolean
  /** 免密态：本地刚成功 **或** 服务器本就是密钥认证（后者不依赖操作时的临时状态） */
  passwordless: boolean
  /** 本会话刚点成功（pwDone）：它决定文案，与「是否显示这一行」是两件事 */
  passwordlessJustDone: boolean
  bbr: {
    /** 当前拥塞控制算法（惰性检测过才有） */
    algo: string | undefined
    checking: boolean
    enabling: boolean
    /** 上一次开启/检测的结果 */
    msg: { ok: boolean; text: string } | undefined
  }
  /** 已部署协议详情（后端节点行）；undefined = 没读到 → 不显示，也不用空值覆盖 */
  deployedProtos: DeployedProtocolDetail[] | undefined
  test: ConnectionTest | undefined
  /** 行内链路摘要（只有已建/接管中的链路才算） */
  linkSummary: string | null
  open: { more: boolean; channels: boolean; upstreams: boolean }
  /** 卡片展开态（部署向导或编辑表单展开中）——展开区内容由父级以 children 注入 */
  expanded: boolean
  onToggleMore: () => void
  onToggleChannels: () => void
  onToggleUpstreams: () => void
  onSpeedTest: () => void
  onTest: () => void
  onPasswordless: () => void
  onBbrEnable: () => void
  onBbrCheck: () => void
  onDeploy: () => void
  onEdit: () => void
  onDelete: () => void
  onClose: () => void
  /** 常驻卡片底部、不随展开区收起（自动重部署的行内进度就落在这里：
   *  编辑保存后表单即关闭，展开区里放不下一个已经收起的进度条）。 */
  footer?: ReactNode
  children?: ReactNode
}

export default function ServerRow({
  server: s,
  speed,
  testing,
  addrBusy,
  onAddrFamily,
  passwordlessBusy,
  passwordless,
  passwordlessJustDone,
  bbr,
  deployedProtos,
  test,
  linkSummary,
  open,
  expanded,
  onToggleMore,
  onToggleChannels,
  onToggleUpstreams,
  onSpeedTest,
  onTest,
  onPasswordless,
  onBbrEnable,
  onBbrCheck,
  onDeploy,
  onEdit,
  onDelete,
  onClose,
  footer,
  children,
}: Props) {
  // BBR 按钮有两种动作：检测出来的算法**不是** bbr 且还没给过结果时，点它是「开启」；
  // 其余（没检测过 / 已经是 bbr / 刚给了结果）都是再检测一次。判据与按钮文案同源，
  // 分开写会在两个分支里各判一遍、迟早改歪一个。
  const bbrCanEnable = bbr.algo != null && bbr.algo !== 'bbr' && !bbr.msg
  return (
    <li className="rounded-lg border border-neutral-800 bg-neutral-900/50">
      <ExpandableCard
        expanded={expanded}
        id={`server-${s.id}`}
        header={
          <div className="p-4">
            <div className="flex items-start justify-between gap-4">
              <div className="min-w-0 space-y-1.5">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium text-neutral-100">{s.name}</span>
                  <span className={`rounded px-1.5 py-0.5 text-xs ${s.kind === 'relay' ? 'bg-sky-950 text-sky-300' : 'bg-blue-950 text-blue-300'}`}>
                    {serverKindLabel(s.kind)}
                  </span>
                  {passwordless && (
                    <span className="rounded bg-emerald-950 px-1.5 py-0.5 text-xs text-emerald-300">已免密</span>
                  )}
                </div>
                <p className="flex flex-wrap items-center gap-x-1 text-xs text-neutral-400">
                  <span className="font-mono">{formatHostPort(effectiveServerHost(s), s.port)}</span>
                  <span className="ml-1 inline-flex overflow-hidden rounded border border-neutral-700" role="group" aria-label="地址族">
                    <button
                      type="button"
                      aria-pressed={s.addr_family === 'v4'}
                      disabled={addrBusy}
                      onClick={() => s.addr_family !== 'v4' && onAddrFamily('v4')}
                      className={`px-1.5 py-0.5 ${s.addr_family === 'v4' ? 'bg-blue-600 text-white' : 'text-neutral-400 hover:bg-neutral-800'} disabled:cursor-not-allowed disabled:opacity-50`}
                    >
                      IPv4
                    </button>
                    <button
                      type="button"
                      aria-pressed={s.addr_family === 'v6'}
                      aria-busy={addrBusy}
                      disabled={addrBusy}
                      title={s.host_v6 ? undefined : '未登记 IPv6 地址：点击后自动从服务器探测，也可以在「编辑」里手动填写'}
                      onClick={() => s.addr_family !== 'v6' && onAddrFamily('v6')}
                      className={`px-1.5 py-0.5 ${s.addr_family === 'v6' ? 'bg-blue-600 text-white' : 'text-neutral-400 hover:bg-neutral-800'} disabled:cursor-not-allowed disabled:opacity-50`}
                    >
                      {addrBusy ? '…' : 'IPv6'}
                    </button>
                  </span>
                  <span>· {s.user} · <span>{s.auth_kind === 'password' ? '密码' : '密钥'}</span>
                    {s.system ? ` · ${s.system}` : ''}
                  </span>
                </p>
                <p className="text-xs text-neutral-500">
                  {s.latency != null ? `延迟 ${s.latency}ms` : '延迟 —'} ·{' '}
                  {s.bandwidth != null ? `下载 ${formatBandwidth(s.bandwidth)}` : '下载 —'} · 上次测速 {formatCheckedAt(s.last_checked_at)}
                </p>
                <div className="space-y-0.5">
                  {speed.error ? (
                    <p className="text-xs text-red-400">{speed.error}</p>
                  ) : speed.done ? (
                    <p className="text-xs text-emerald-400">已完成</p>
                  ) : null}
                  {bbr.algo != null && <p className="text-xs text-neutral-500">当前算法：{bbr.algo}</p>}
                  {bbr.msg && (
                    <p className={`text-xs ${bbr.msg.ok ? 'text-emerald-400' : 'text-red-400'}`}>{bbr.msg.text}</p>
                  )}
                  {linkSummary && <p className="text-xs text-emerald-400">{linkSummary}</p>}
                  <DeployedProtocolList protocols={deployedProtos ?? []} />
                  {test && !test.ok && <p className="text-xs text-red-400">连通测试失败：{test.message}</p>}
                  {test && test.ok && <p className="text-xs text-emerald-400">连通测试成功：{test.system}</p>}
                  {passwordless && (
                    <p className="text-xs text-emerald-400">
                      {passwordlessJustDone ? '免密成功：已切换密钥认证' : '已免密（密钥认证）'}
                    </p>
                  )}
                </div>
              </div>
              <div className="flex shrink-0 flex-wrap justify-end gap-2">
                <button type="button" disabled={speed.busy} onClick={onSpeedTest} className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-sky-300 hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50">
                  {speed.busy ? '测速中…' : '测速'}
                </button>
                <button type="button" disabled={testing} onClick={onTest} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700 disabled:opacity-50">
                  {testing ? '测试中…' : '连通测试'}
                </button>
                <button type="button" disabled={passwordlessBusy} onClick={onPasswordless} className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-emerald-300 hover:bg-neutral-700 disabled:opacity-50">
                  {passwordlessBusy ? '免密中…' : '一键免密'}
                </button>
                <button
                  type="button"
                  aria-expanded={open.more}
                  onClick={onToggleMore}
                  className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-neutral-300 hover:bg-neutral-700"
                >
                  {open.more ? '收起' : '更多'}
                </button>
              </div>
            </div>
            {open.more && (
              <div className="mt-3 flex flex-wrap items-center gap-2 border-t border-neutral-800 pt-3">
                {bbr.checking ? (
                  <button type="button" disabled className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-amber-300 disabled:opacity-50">
                    检测中…
                  </button>
                ) : (
                  <button
                    type="button"
                    disabled={bbr.enabling}
                    onClick={() => {
                      if (bbrCanEnable) onBbrEnable()
                      else onBbrCheck()
                    }}
                    className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-amber-300 hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
                  >
                    {bbr.enabling ? '开启中…' : bbrCanEnable ? '开启 BBR' : 'BBR'}
                  </button>
                )}
                <button
                  type="button"
                  onClick={onToggleChannels}
                  className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-sky-300 hover:bg-neutral-700"
                >
                  {open.channels ? '收起通道' : '通道'}
                </button>
                <button
                  type="button"
                  onClick={onToggleUpstreams}
                  className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-sky-300 hover:bg-neutral-700"
                >
                  {open.upstreams ? '收起上游出口' : '上游出口'}
                </button>
                {s.kind === 'landing' && (
                  <button type="button" onClick={onDeploy} className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600">
                    部署落地机
                  </button>
                )}
                <button type="button" onClick={onEdit} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
                  编辑
                </button>
                <ConfirmButton label="删除" confirmLabel="确认删除" busyLabel="删除中…" busy={false} onConfirm={onDelete} />
              </div>
            )}
            {open.channels && <ChannelPanel serverId={s.id} sshPort={s.port} />}
            {open.upstreams && <UpstreamPanel serverId={s.id} serverHost={effectiveServerHost(s)} />}
          </div>
        }
        onClose={onClose}
      >
        {children}
      </ExpandableCard>
      {footer != null && <div className="px-4 pb-4">{footer}</div>}
    </li>
  )
}
