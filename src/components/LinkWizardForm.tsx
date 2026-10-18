// 建链向导基础表单：中转/落地机下拉（按 kind 过滤，空则提示去服务器页标注）、
// 模式 radio（切换时重置主端口默认值）、端口/落地端口输入与收敛开关。
// 受控组件：值与回调由向导持有；disabled 用于任务执行期锁表单。
import type { Server } from '../api/types'

type Mode = 'forward' | 'peer'

type ForwardMode = 'single' | 'same_port'

interface Props {
  relayServers: Server[]
  landingServers: Server[]
  relayId: number | null
  landingId: number | null
  mode: Mode
  port: number
  landingPort: number
  converged: boolean
  forwardMode: ForwardMode
  disabled: boolean
  onRelayId: (v: number | null) => void
  onLandingId: (v: number | null) => void
  onMode: (v: Mode) => void
  onPort: (v: number) => void
  onLandingPort: (v: number) => void
  onConverged: (v: boolean) => void
  onForwardMode: (v: ForwardMode) => void
}

export default function LinkWizardForm({
  relayServers,
  landingServers,
  relayId,
  landingId,
  mode,
  port,
  landingPort,
  converged,
  forwardMode,
  disabled,
  onRelayId,
  onLandingId,
  onMode,
  onPort,
  onLandingPort,
  onConverged,
  onForwardMode,
}: Props) {
  // 整机转发只在模式 A 有意义（模式 B 是 WG 对等，本就不做端口转发）
  const samePort = mode === 'forward' && forwardMode === 'same_port'
  return (
    <>
      <div className="flex gap-2">
        <label className="block flex-1 text-sm">
          中转机
          <select
            className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
            value={relayId ?? ''}
            disabled={disabled}
            onChange={(e) => onRelayId(e.target.value === '' ? null : Number(e.target.value))}
          >
            <option value="">选择中转机</option>
            {relayServers.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}（{s.host}）
              </option>
            ))}
          </select>
        </label>
        <label className="block flex-1 text-sm">
          落地机
          <select
            className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
            value={landingId ?? ''}
            disabled={disabled}
            onChange={(e) => onLandingId(e.target.value === '' ? null : Number(e.target.value))}
          >
            <option value="">选择落地机</option>
            {landingServers.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}（{s.host}）
              </option>
            ))}
          </select>
        </label>
      </div>
      {relayServers.length === 0 && <p className="text-sm text-neutral-500">请先在服务器页把类型标为中转</p>}
      {landingServers.length === 0 && <p className="text-sm text-neutral-500">请先在服务器页把类型标为落地</p>}

      <div className="flex gap-2">
        <div className="block text-sm">
          <span className="block">模式</span>
          <span className="mt-1 flex gap-4">
            <label className="flex items-center gap-1">
              <input type="radio" name="link-mode" checked={mode === 'forward'} disabled={disabled} onChange={() => { onMode('forward'); onPort(443) }} />
              端口转发（模式 A）
            </label>
            <label className="flex items-center gap-1">
              <input type="radio" name="link-mode" checked={mode === 'peer'} disabled={disabled} onChange={() => { onMode('peer'); onPort(51820) }} />
              WG 对等（模式 B）
            </label>
          </span>
        </div>
        <label className="block w-28 text-sm">
          端口
          <input type="number" className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 disabled:opacity-50" value={port} disabled={disabled || samePort} onChange={(e) => onPort(Number(e.target.value))} />
        </label>
        <label className="block w-28 text-sm">
          落地端口
          <input type="number" className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 disabled:opacity-50" value={landingPort} disabled={disabled || samePort} onChange={(e) => onLandingPort(Number(e.target.value))} />
        </label>
      </div>
      {mode === 'forward' && (
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={samePort}
            disabled={disabled}
            onChange={(e) => onForwardMode(e.target.checked ? 'same_port' : 'single')}
          />
          整机转发（按原端口覆盖这台落地机的全部协议）
        </label>
      )}
      {samePort && (
        <p className="text-xs text-neutral-500">
          端口取自该落地机上的自建节点，无需手填：节点有几个端口就转发几个，客户端仍连中转机的原端口号。
        </p>
      )}
      <label className="flex items-center gap-2 text-sm">
        <input type="checkbox" checked={converged} disabled={disabled} onChange={(e) => onConverged(e.target.checked)} />
        建链后收敛落地机监听到 WG 内网
      </label>
    </>
  )
}
