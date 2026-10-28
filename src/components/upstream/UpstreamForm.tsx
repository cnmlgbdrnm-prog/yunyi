// 上游出口的新增/编辑共用表单（同一套字段，editing=true 时是编辑面）。
//
// 「入口协议」是 本机 → 中转机 这一段：优先复用服务器上已部署的主入站
// （VLESS/Trojan/VMess/Hysteria2/TUIC），不再为出口单独开端口；若该协议尚未部署，
// 选它会让保存后的部署先创建主入口，再接入住宅出口。旧专用实例（legacy）只为
// 存量行保留，新出口不再提供。
import { useState } from 'react'
import type { UpstreamEntryOptions } from '../../api/types'
import { REALITY_DEST_GROUPS, findRealityDest } from '../../utils/realityDest'
import { defaultUpstreamName, parseProxyInput } from '../../utils/proxyUrl'
import { ENTRY_PROTO_LABEL } from '../../utils/upstreamEntry'

export type UpstreamFormValues = {
  name: string
  proto: string
  host: string
  port: string
  username: string
  password: string
  listenPort: string
  realityDest: string
  /** node:<node_id> | proto:<protocol> | legacy | ''（未选） */
  entry: string
}

const PROTO_OPTIONS = ['socks5', 'http']
const field = 'mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm'

function entryHint(entry: string): string {
  if (entry.startsWith('node:')) {
    return '复用已部署的入口：不新开端口，也不会多部署一条协议；以后在「编辑」里可以随时换到别的已部署协议。'
  }
  if (entry.startsWith('proto:')) {
    return '该协议尚未部署：保存后会先在服务器上部署这个主入口，再把出口接到它上面。'
  }
  if (entry === 'legacy') {
    return '旧模式：为这条出口单独部署一个 VLESS+REALITY 实例（独立端口）。存量出口保持可用，新出口不建议选它。'
  }
  return '选择 本机 → 中转机 这一段使用的入口协议。'
}

function entryLabel(entry: string, options: UpstreamEntryOptions | null): string {
  if (entry.startsWith('node:')) {
    const id = entry.slice('node:'.length)
    const found = options?.deployed.find((o) => o.node_id === id)
    return found ? `复用 ${found.label}` : '复用入口（节点已失效，保存后按协议恢复）'
  }
  if (entry.startsWith('proto:')) {
    const proto = entry.slice('proto:'.length)
    return `${ENTRY_PROTO_LABEL[proto] ?? proto} · 尚未部署（保存后先部署）`
  }
  if (entry === 'legacy') return '专用实例（旧，不推荐）'
  return '— 选择入口协议 —'
}

export default function UpstreamForm({
  values,
  onChange,
  editing,
  adding,
  options,
  onSubmit,
  onCancel,
}: {
  values: UpstreamFormValues
  onChange: <K extends keyof UpstreamFormValues>(key: K, value: UpstreamFormValues[K]) => void
  editing: boolean
  adding: boolean
  options: UpstreamEntryOptions | null
  onSubmit: () => void
  onCancel: () => void
}) {
  // 快速粘贴：服务商给的是「一整条带认证的链接」时，一次拆进下面各字段，
  // 省掉逐个填主机/端口/用户名/密码。解析失败只就地提示，不动已填内容。
  const [quick, setQuick] = useState('')
  const [quickError, setQuickError] = useState<string | null>(null)
  const [quickOk, setQuickOk] = useState<string | null>(null)

  const fillFromQuick = () => {
    setQuickError(null)
    setQuickOk(null)
    try {
      const parsed = parseProxyInput(quick, values.proto)
      onChange('proto', parsed.proto)
      onChange('host', parsed.host)
      onChange('port', String(parsed.port))
      onChange('username', parsed.username)
      onChange('password', parsed.password)
      // 只补空名称：编辑既有出口时用户可能已经起好名字，不能覆盖
      if (!values.name.trim()) onChange('name', defaultUpstreamName(parsed))
      setQuickOk(`已填入 ${parsed.proto} ${parsed.host}:${parsed.port}`)
      setQuick('')
    } catch (e) {
      setQuickError(e instanceof Error ? e.message : String(e))
    }
  }

  const deployed = options?.deployed ?? []
  const supported = options?.supported ?? []
  const deployedProtos = new Set(deployed.map((o) => o.protocol))
  const undeployed = supported.filter((p) => !deployedProtos.has(p))
  const allowLegacy = values.entry === 'legacy'
  const known = new Set<string>([
    ...deployed.map((o) => `node:${o.node_id}`),
    ...undeployed.map((p) => `proto:${p}`),
    ...(allowLegacy ? ['legacy'] : []),
  ])
  const legacy = values.entry === 'legacy'

  return (
    <div className="mt-2 grid grid-cols-2 gap-2">
      <div className="col-span-2 rounded border border-neutral-800 bg-neutral-900/40 p-2">
        <label className="text-xs text-neutral-400">
          快速粘贴（代理链接、curl 命令，或「主机:端口:用户名:密码」）
          <div className="mt-1 flex gap-2">
            <input
              aria-label="快速粘贴"
              className="flex-1 rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm"
              value={quick}
              placeholder="socks5://用户:密码@主机:端口"
              onChange={(e) => {
                setQuick(e.target.value)
                setQuickError(null)
                setQuickOk(null)
              }}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  fillFromQuick()
                }
              }}
            />
            <button
              type="button"
              onClick={fillFromQuick}
              className="rounded bg-neutral-800 px-3 py-1 text-xs text-sky-300 hover:bg-neutral-700"
            >
              填入
            </button>
          </div>
        </label>
        {quickError && (
          <p role="alert" className="mt-1 text-xs text-red-400">
            {quickError}
          </p>
        )}
        {quickOk && (
          <p role="status" className="mt-1 text-xs text-emerald-400">
            {quickOk}
          </p>
        )}
      </div>

      <label className="col-span-2 text-xs text-neutral-400">
        入口协议（本机 → 中转机）
        <select
          aria-label="入口协议"
          className={field}
          value={values.entry}
          disabled={options == null}
          onChange={(e) => onChange('entry', e.target.value)}
        >
          {options == null && <option value="">读取入口协议中…</option>}
          {options != null && values.entry === '' && (
            <option value="">— 选择入口协议 —</option>
          )}
          {options != null && values.entry !== '' && !known.has(values.entry) && (
            <option value={values.entry}>{entryLabel(values.entry, options)}</option>
          )}
          {deployed.length > 0 && (
            <optgroup label="复用已部署的入口">
              {deployed.map((o) => (
                <option key={`node:${o.node_id}`} value={`node:${o.node_id}`}>
                  {`复用 ${o.label}`}
                </option>
              ))}
            </optgroup>
          )}
          {undeployed.length > 0 && (
            <optgroup label="尚未部署（保存后自动部署）">
              {undeployed.map((p) => (
                <option key={`proto:${p}`} value={`proto:${p}`}>
                  {`${ENTRY_PROTO_LABEL[p] ?? p} · 尚未部署`}
                </option>
              ))}
            </optgroup>
          )}
          {allowLegacy && (
            <option value="legacy">专用实例（旧，不推荐）</option>
          )}
        </select>
        <p className="mt-1 text-neutral-500">{entryHint(values.entry)}</p>
      </label>

      <label className="text-xs text-neutral-400">
        出口名称
        <input
          aria-label="出口名称"
          className={field}
          value={values.name}
          // 名称参与写回节点的命名（<服务器>-<出口名>）。保存只写库；重新部署时
          // 部署 job 会按同一个 node_id 用新名字覆盖那只节点。
          onChange={(e) => onChange('name', e.target.value)}
        />
        {editing && <span className="text-neutral-500">改名后点「重新部署」同步到节点名</span>}
      </label>
      <label className="text-xs text-neutral-400">
        住宅代理协议
        <select
          aria-label="上游协议"
          className={field}
          value={values.proto}
          onChange={(e) => onChange('proto', e.target.value)}
        >
          {PROTO_OPTIONS.map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
      </label>
      <label className="text-xs text-neutral-400">
        上游地址
        <input
          aria-label="上游地址"
          className={field}
          value={values.host}
          onChange={(e) => onChange('host', e.target.value)}
        />
      </label>
      <label className="text-xs text-neutral-400">
        上游端口
        <input
          aria-label="上游端口"
          className={field}
          value={values.port}
          onChange={(e) => onChange('port', e.target.value)}
        />
      </label>
      <label className="text-xs text-neutral-400">
        用户名
        <input
          aria-label="用户名"
          className={field}
          value={values.username}
          onChange={(e) => onChange('username', e.target.value)}
        />
      </label>
      <label className="text-xs text-neutral-400">
        密码
        <input
          aria-label="密码"
          type="password"
          className={field}
          value={values.password}
          placeholder={editing ? '留空＝不改' : ''}
          onChange={(e) => onChange('password', e.target.value)}
        />
      </label>
      {legacy && (
        <>
          <label className="text-xs text-neutral-400">
            监听端口
            <input
              aria-label="监听端口"
              className={field}
              value={values.listenPort}
              onChange={(e) => onChange('listenPort', e.target.value)}
            />
          </label>
          <div className="col-span-2 text-xs text-neutral-400">
            伪装域名（专用实例的 REALITY 镜像目标，留默认即可）
            {/* 下拉只是「快捷填入」：输入框始终是事实源，两者都改得 */}
            <select
              aria-label="伪装域名预设"
              className={field}
              /* 手填了清单外的域名时回落到占位项，避免下拉显示与输入框不一致 */
              value={findRealityDest(values.realityDest) ? values.realityDest.trim() : ''}
              onChange={(e) => {
                if (e.target.value !== '') onChange('realityDest', e.target.value)
              }}
            >
              <option value="">— 选择常用域名 —</option>
              {REALITY_DEST_GROUPS.map((g) => (
                <optgroup key={g.label} label={g.label}>
                  {g.options.map((o) => (
                    <option key={o.domain} value={o.domain}>
                      {`${o.ok ? '✓' : '✗'} ${o.domain}`}
                      {o.note ? ` — ${o.note}` : ''}
                    </option>
                  ))}
                </optgroup>
              ))}
            </select>
            <input
              aria-label="伪装域名"
              className={field}
              value={values.realityDest}
              onChange={(e) => onChange('realityDest', e.target.value)}
            />
            {/* 选中项的标注：下拉里每个域名都带了实测结论，这里把当前这条展开说清楚 */}
            {findRealityDest(values.realityDest)?.note && (
              <p
                className={`mt-1 ${
                  findRealityDest(values.realityDest)?.warn
                    ? 'text-amber-400'
                    : 'text-neutral-500'
                }`}
              >
                {findRealityDest(values.realityDest)?.note}
              </p>
            )}
          </div>
        </>
      )}
      {values.proto === 'http' && (
        <p className="col-span-2 text-xs text-amber-400">
          HTTP 上游不支持 UDP（CONNECT 只能承载 TCP）——要 UDP 请选 socks5。
        </p>
      )}
      <div className="col-span-2 flex gap-2">
        <button
          type="button"
          disabled={adding || values.entry === ''}
          onClick={onSubmit}
          className="rounded bg-sky-800 px-3 py-1 text-xs text-sky-100 hover:bg-sky-700 disabled:opacity-50"
        >
          {editing ? '保存改动' : adding ? '部署中…' : '保存并部署'}
        </button>
        {editing && (
          <button
            type="button"
            onClick={onCancel}
            className="rounded bg-neutral-800 px-3 py-1 text-xs text-neutral-300 hover:bg-neutral-700"
          >
            取消
          </button>
        )}
      </div>
    </div>
  )
}
