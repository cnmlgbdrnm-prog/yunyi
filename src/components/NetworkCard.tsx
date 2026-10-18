// 仪表盘的「网络」卡：一键开关、出口节点选择、TUN 开关，以及一整排**如实说话**的状态行。
//
// 这张卡的规矩（改文案前先读，每条都是踩过坑的）：
// - 会话阻断没生效时**如实说**，但只给一行安静的标记，整段原因挪到设置页——
//   不报等于把 fail-open 藏起来，整段摆上来用户嫌丑（原话「看着丑死了」）。
//   措辞刻意不写成「失败」：网络本身是好的，这只是一层增益没上。
// - 开启网络时接管了几条「开启前就已建立的直连」是**纯事实一行**：0 条是常态，
//   那就什么都不说；只有真接管了才出一声。
// - 「没做成」也出声，但**不重复说同一件事**：服务不可达/旧构建上面那行已经说了，
//   只有「阻断那层是好的、偏偏接管没做成」才出这一行。
// - 出口节点的 label 可访问名必须正好是「出口节点」：把实际出口混进 label 会让
//   按名取控件的用法（测试、读屏）跟着出口变而失效。
import type { FirewallState, NetStatus, Node } from '../api/types'

/** 下拉框里「自动选择」这一项的值。它同时是送给后端的节点名——`手动选择` 组的首项
 *  就是 `自动选择`，后端据此把出口交回「测速最快」。字面量必须与后端 `AUTO_GROUP` 一致。 */
export const AUTO_OPTION = '自动选择'

/**
 * 阻断状态里哪些要出说明。
 *
 * `off` 不出：那是「没有要挡的程序」或「服务侧主动撤了」（TUN 开着时本来就该撤），
 * 都是正常态。`applied` 更不出。其余都是「本该挡住却没挡住」，必须让用户看见——
 * 这个功能存在的意义就是不再静默。
 */
export function firewallWarning(state: FirewallState): boolean {
  // 白名单而不是「非 off/applied」：后端比前端旧时（Vite dev server 配旧 exe 是本仓库
  // 踩过的形态）这个字段整个是 undefined，黑名单写法会渲染出一条**空文案**的说明
  return (
    state === 'partial' ||
    state === 'denied' ||
    state === 'unavailable' ||
    state === 'unsupported'
  )
}

/**
 * 每种阻断状态的**短标记**。**一律不写成「失败」**：网络本身是好的，这只是一层
 * 增益没上——`denied`/`unavailable` 时用户的网完全正常，把它说成失败只会制造恐慌。
 *
 * 仪表盘上只给这一小行，原因（哪些程序没挡住、为什么）在设置页：整段话摆在
 * 网络卡上用户嫌丑，但删掉不提又是把 fail-open 藏起来——所以是「挪地方」不是「不报」。
 */
export function firewallLabel(state: FirewallState): string {
  switch (state) {
    case 'partial':
      return '部分未挡住'
    case 'denied':
      return '被防火墙拒绝'
    case 'unavailable':
      return '服务不可用'
    case 'unsupported':
      return '系统不支持'
    default:
      return ''
  }
}

interface Props {
  net: NetStatus | null
  netBusy: boolean
  netError: string | null
  netSelectBusy: boolean
  netTunBusy: boolean
  proxyRepairBusy: boolean
  /** 出口选择模式：auto = 交给「测速最快」，manual = 钉住某个节点 */
  exitMode: 'auto' | 'manual'
  /** 后端回读到的当前出口（auto 模式下它才是实际上生效的那个） */
  selectedNode: string | null
  /** 下拉项：启用且非 ssr 的节点（客户端 config 的口径） */
  clientNodes: Node[]
  /** node_id → 隐私线在 mihomo 里的真实名称（有 onion 地址的才有） */
  privateNames: Map<string, string>
  /** 系统代理没指向本应用：浏览器可能绕过隐私线路 */
  proxyDetached: boolean
  onToggle: () => void
  onSelect: (name: string) => void
  onTunToggle: (enable: boolean) => void
  onRepairProxy: () => void
  /** 两条标记行都指向设置页的完整清单 */
  onOpenSettings: () => void
}

export default function NetworkCard({
  net,
  netBusy,
  netError,
  netSelectBusy,
  netTunBusy,
  proxyRepairBusy,
  exitMode,
  selectedNode,
  clientNodes,
  privateNames,
  proxyDetached,
  onToggle,
  onSelect,
  onTunToggle,
  onRepairProxy,
  onOpenSettings,
}: Props) {
  return (
    <div className="rounded border border-neutral-800 bg-neutral-900 p-4">
      <div className="flex items-center justify-between gap-3">
        <div>
          <p className="text-sm text-neutral-400">网络</p>
          <p className="mt-1 text-xl font-bold">
            {net === null ? '状态未知' : net.running ? `运行中 · 端口 ${net.port}` : '未开启'}
          </p>
          <p className="mt-1 text-xs text-neutral-500">
            {net === null
              ? '后端状态查询失败，可重启应用后再试'
              : net.running
              ? proxyDetached
                ? '系统代理未接入 · 浏览器可能没有经过本应用'
                : '系统代理已接入 · 智能分流（国内直连/国外走节点）'
                : '一键开启：国内直连，国外经落地机节点'}
          </p>
          {net?.running && (
            // 「当前：」在 label **外**：label 的可访问名要正好是「出口节点」，
            // 把实际出口混进去会让按名取控件的用法（测试、读屏）跟着出口变而失效
            <div className="mt-2 flex items-center gap-2 text-xs text-neutral-400">
              <label className="flex items-center gap-2">
                出口节点
                <select
                  className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm"
                  value={exitMode === 'auto' ? AUTO_OPTION : selectedNode || AUTO_OPTION}
                  disabled={netSelectBusy}
                  onChange={(e) => onSelect(e.target.value)}
                >
                  <option value={AUTO_OPTION}>自动选择（测速最快）</option>
                  {clientNodes.map((n) => (
                    <option key={n.id} value={privateNames.get(n.id) ?? n.name}>
                      {privateNames.get(n.id) ?? n.name}
                    </option>
                  ))}
                </select>
              </label>
              {netSelectBusy && <span>正在验证出口…</span>}
              {exitMode === 'auto' && selectedNode && selectedNode !== AUTO_OPTION && (
                <span className="text-neutral-500">当前：{selectedNode}</span>
              )}
            </div>
          )}
          {net?.running && (
            <div className="mt-2">
              <label className="flex items-center gap-2 text-xs text-neutral-400">
                <input
                  type="checkbox"
                  checked={net.tun}
                  disabled={!net.via_service || netTunBusy}
                  onChange={(e) => onTunToggle(e.target.checked)}
                />
                TUN 全局模式
              </label>
              <p className="mt-1 text-xs text-neutral-500">
                {!net.via_service
                  ? '需先安装网络服务（到「设置」安装），再重新开启网络'
                  : net.tun
                    ? '已全局接管流量'
                    : '开启后全局接管流量'}
              </p>
            </div>
          )}
        </div>
        <button
          type="button"
          onClick={onToggle}
          disabled={net === null || netBusy}
          className={`rounded px-3 py-1.5 text-sm text-white disabled:opacity-50 ${
            net?.running ? 'bg-neutral-700 hover:bg-neutral-600' : 'bg-emerald-700 hover:bg-emerald-600'
          }`}
        >
          {netBusy
            ? net?.running
              ? '关闭中…'
              : '开启中…'
            : net?.running
              ? '关闭网络'
              : '开启网络'}
        </button>
      </div>
      {netError && (
        <p role="alert" className="mt-2 rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {netError}
        </p>
      )}
      {net?.running && net.degraded_geo && (
        <p className="mt-2 rounded border border-amber-700 bg-amber-950 px-3 py-2 text-sm text-amber-200">
          分流数据缺失（geoip/geosite 未就绪）：当前仅国内直连兜底，国外节点分流待数据就绪后生效
        </p>
      )}
      {net?.running && net.tor && net.tor !== 'off' && (
        <p
          role={net.tor === 'failed' ? 'alert' : undefined}
          className={`mt-2 text-xs ${net.tor === 'failed' ? 'text-red-300' : 'text-neutral-400'}`}
        >
          Tor 隐私入口：{net.tor === 'ready' ? '已就绪' : net.tor === 'starting' ? '正在建立' : net.tor === 'external' ? '复用现有 Tor' : '不可用'}
          {net.tor_detail ? ` · ${net.tor_detail}` : ''}
        </p>
      )}
      {proxyDetached && (
        <div role="alert" className="mt-2 flex items-center gap-2 text-xs text-amber-300">
          <span>系统代理未指向本应用；浏览器可能绕过隐私线路。</span>
          <button type="button" disabled={proxyRepairBusy} onClick={onRepairProxy} className="underline disabled:opacity-50">
            {proxyRepairBusy ? '恢复中…' : '恢复系统代理'}
          </button>
        </div>
      )}
      {net?.running && !net.tun && (
        <p className="mt-2 text-xs text-amber-300">
          系统代理模式无法接管所有程序；未被防火墙规则覆盖的程序仍可能直连公网。请开启 TUN 全局模式以接管它们。
        </p>
      )}
      {/* 会话级阻断没生效时如实说：非 TUN 下关注程序的公网直连
          本该被挡住，没挡住就得让用户看见，而不是让面板一片绿。
          但只给一行安静的标记——整段原因摆在网络卡上太吵（用户原话「看着丑死了」），
          而**不报**等于把 fail-open 藏起来。所以：这里带一句，完整原因在设置页。
          措辞刻意不写成「失败」——网络本身是好的，这只是一层增益没上。 */}
      {net?.running && firewallWarning(net.firewall) && (
        <button
          type="button"
          onClick={onOpenSettings}
          title={net.firewall_detail ?? undefined}
          className="mt-2 block text-xs text-neutral-500 underline decoration-dotted hover:text-neutral-300"
        >
          会话阻断：{firewallLabel(net.firewall)} · 详情见「设置」
        </button>
      )}
      {/* 开启网络时把「开启前就已建立的直连」掐掉了几条——纯事实一行。
          0 条是常态（没有要接管的），那就什么都不说；只有真接管了才出一声。 */}
      {net?.running && (net.takeover_killed ?? 0) > 0 && (
        <p className="mt-2 text-xs text-neutral-500">
          已接管 {net.takeover_killed} 条开启前的连接
        </p>
      )}
      {/* 没做成时也要出声（fail-open 不藏），但**不重复说同一件事**：服务不可达/
          是旧构建这种情况上面那行已经说了，完整原因在设置页，这里再报一遍只是吵。
          所以只有「阻断那层是好的、偏偏接管没做成」才显示这一行。 */}
      {net?.running &&
        net.takeover_killed == null &&
        net.takeover_detail &&
        !firewallWarning(net.firewall) && (
          <button
            type="button"
            onClick={onOpenSettings}
            title={net.takeover_detail}
            className="mt-2 block text-xs text-neutral-500 underline decoration-dotted hover:text-neutral-300"
          >
            开启前的连接未接管 · 详情见「设置」
          </button>
        )}
    </div>
  )
}
