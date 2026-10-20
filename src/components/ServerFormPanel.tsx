// 服务器表单面板：顶部「新建」与卡片内「编辑」两处共用同一份字段 JSX。
//
// 为什么表单状态留在父级（Servers）：编辑态要与卡片的展开态互斥（同一个 expand 单状态
// 模型保证），状态只留一份才不会两处打架；本组件只渲染字段、把整份表单回传，不持副本。
// 顶部新建时 onCancel 只收起表单，卡片内编辑时还要顺带复位 editingId 与展开态——
// 那是两份不同的收尾，由父级注入。

import Ipv6DetectButton from './Ipv6DetectButton'

export interface ServerFormState {
  name: string
  host: string
  /** 可选的 IPv6 字面量（不带方括号）；留空表示这台服务器只走 IPv4。 */
  hostV6: string
  port: number
  user: string
  authKind: 'password' | 'key'
  kind: 'relay' | 'landing'
  notes: string
  credential: string
  /** 出口地址族：VPS → 目标域名时优先/限定哪一族。与入口（host/hostV6）独立。 */
  egressFamily: 'v4' | 'v6'
  /** 分享域名（裸域名）；空串 = 未配置。新分享强制 HTTPS。 */
  shareDomain: string
}

export const EMPTY_SERVER_FORM: ServerFormState = {
  name: '',
  host: '',
  hostV6: '',
  port: 22,
  user: 'root',
  authKind: 'password',
  kind: 'landing',
  notes: '',
  credential: '',
  egressFamily: 'v4',
  shareDomain: '',
}

interface Props {
  form: ServerFormState
  /** 编辑态：凭据留空表示保留原凭据，故要多提示一句 */
  editing: boolean
  /** 编辑已有服务器时传入：显示「自动探测」，新建尚未入库没有可连的凭据 */
  serverId?: number
  onChange: (form: ServerFormState) => void
  onSave: () => void
  onCancel: () => void
}

export default function ServerFormPanel({ form: f, editing, serverId, onChange, onSave, onCancel }: Props) {
  return (
    <div className="space-y-2">
      <label className="block text-sm">
        名称
        <input className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={f.name} onChange={(e) => onChange({ ...f, name: e.target.value })} />
      </label>
      <div className="flex gap-2">
        <label className="block flex-1 text-sm">
          主机
          <input className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={f.host} onChange={(e) => onChange({ ...f, host: e.target.value })} />
        </label>
        <label className="block w-24 text-sm">
          端口
          <input type="number" className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={f.port} onChange={(e) => onChange({ ...f, port: Number(e.target.value) })} />
        </label>
      </div>
      <div className="block text-sm">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <label htmlFor="server-host-v6">IPv6 地址（可选）</label>
          {serverId != null && (
            <Ipv6DetectButton
              serverId={serverId}
              onDetected={(address) => onChange({ ...f, hostV6: address })}
            />
          )}
        </div>
        <input id="server-host-v6" aria-label="IPv6 地址" className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 font-mono text-xs" value={f.hostV6} onChange={(e) => onChange({ ...f, hostV6: e.target.value })} placeholder="2001:db8::1（只填字面量，不带方括号）" />
      </div>
      <div className="flex gap-2">
        <label className="block flex-1 text-sm">
          登录用户
          <input className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={f.user} onChange={(e) => onChange({ ...f, user: e.target.value })} />
        </label>
        <label className="block w-32 text-sm">
          认证方式
          <select className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={f.authKind} onChange={(e) => onChange({ ...f, authKind: e.target.value as 'password' | 'key', credential: '' })}>
            <option value="password">密码</option>
            <option value="key">密钥</option>
          </select>
        </label>
        <label className="block w-32 text-sm">
          类型
          <select className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={f.kind} onChange={(e) => onChange({ ...f, kind: e.target.value as 'relay' | 'landing' })}>
            <option value="landing">落地</option>
            <option value="relay">中转</option>
          </select>
        </label>
      </div>
      {f.authKind === 'password' ? (
        <label className="block text-sm">
          密码
          <input type="password" className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={f.credential} onChange={(e) => onChange({ ...f, credential: e.target.value })} />
        </label>
      ) : (
        <label className="block text-sm">
          私钥 PEM
          <textarea rows={5} className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 font-mono text-xs" value={f.credential} onChange={(e) => onChange({ ...f, credential: e.target.value })} placeholder="-----BEGIN OPENSSH PRIVATE KEY-----" />
        </label>
      )}
      {serverId != null && (
        <div className="block text-sm">
          <label htmlFor="server-egress-family">出站地址族（VPS 访问目标）</label>
          <select
            id="server-egress-family"
            aria-label="出站地址族"
            className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
            value={f.egressFamily}
            onChange={(e) => onChange({ ...f, egressFamily: e.target.value as 'v4' | 'v6' })}
          >
            <option value="v4">v4（默认，VPS 以 IPv4 访问目标）</option>
            <option value="v6">v6（VPS 以 IPv6 访问目标，需 VPS 自身有 v6 出口）</option>
          </select>
          <p className="mt-1 text-xs text-neutral-500">
            只影响 VPS → 目标域名的解析与建连；字面 IP 目标与本机分流不受影响。Shadowsocks
            只能「优先 v6，可能回落 v4」；VPS 没有可用 v6 出口时，域名目标会直接失败。
            保存后若该机已部署协议会自动重部署，进度显示在服务器行内。
          </p>
        </div>
      )}
      <label className="block text-sm">
        分享域名（可选）
        <input
          aria-label="分享域名"
          className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 font-mono text-xs"
          value={f.shareDomain}
          onChange={(e) => onChange({ ...f, shareDomain: e.target.value })}
          placeholder="share.example.com（新分享强制 HTTPS，需解析到本机）"
        />
      </label>
      {editing && <p className="text-xs text-neutral-500">编辑时凭据留空表示保留原凭据</p>}
      <label className="block text-sm">
        备注
        <input className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1" value={f.notes} onChange={(e) => onChange({ ...f, notes: e.target.value })} />
      </label>
      <div className="flex gap-2">
        <button type="button" onClick={onSave} className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-500">
          保存
        </button>
        <button type="button" onClick={onCancel} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
          取消
        </button>
      </div>
    </div>
  )
}
