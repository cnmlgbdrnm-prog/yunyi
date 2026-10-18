// 建链向导探测结果面板：展示握手状态与探测说明，三态分流
// （已握手→接管回读表单（wg_port/subnet/forward_port 可改）；单端有 WG→提示重建；
// 未配置→提示直接建链）。纯展示组件，值由父级持有，改动经回调回写。
import type { ProbeResult } from '../api/types'

interface Props {
  probe: ProbeResult
  mode: 'forward' | 'peer'
  wgPort: number
  subnet: string
  forwardPort: number
  disabled: boolean
  onWgPort: (v: number) => void
  onSubnet: (v: string) => void
  onForwardPort: (v: number) => void
}

export default function LinkProbePanel({ probe, mode, wgPort, subnet, forwardPort, disabled, onWgPort, onSubnet, onForwardPort }: Props) {
  return (
    <div className="space-y-1 rounded border border-neutral-800 p-3 text-sm">
      <p className="text-neutral-300">探测结果：{probe.handshake ? '两端已握手，可接管' : '未握手'}</p>
      {probe.message !== '' && <p className="text-yellow-300/80">{probe.message}</p>}
      {probe.handshake ? (
        <>
          <div className="flex gap-2 pt-1">
            <label className="block w-28 text-sm">
              WG 端口
              <input type="number" className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={wgPort} disabled={disabled} onChange={(e) => onWgPort(Number(e.target.value))} />
            </label>
            <label className="block flex-1 text-sm">
              网段
              <input className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={subnet} placeholder="10.9.x.0/24" disabled={disabled} onChange={(e) => onSubnet(e.target.value)} />
            </label>
            {mode === 'forward' && (
              <label className="block w-28 text-sm">
                转发端口
                <input type="number" className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={forwardPort} disabled={disabled} onChange={(e) => onForwardPort(Number(e.target.value))} />
              </label>
            )}
          </div>
          <p className="text-xs text-neutral-500">以下为探测回读的现有配置，可修改后接管</p>
        </>
      ) : !probe.relay_wg && !probe.landing_wg ? (
        <p className="text-neutral-400">两端均未检测到 WireGuard 配置，可直接建链</p>
      ) : (
        <p className="text-yellow-300/80">一端已有 WG 配置但未握手，可重新建链覆盖</p>
      )}
    </div>
  )
}
