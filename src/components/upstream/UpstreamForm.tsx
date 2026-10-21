// 上游出口的新增/编辑共用表单（同一套字段，editing=true 时是编辑面）。
import { REALITY_DEST_GROUPS, findRealityDest } from '../../utils/realityDest'

export type UpstreamFormValues = {
  name: string
  proto: string
  host: string
  port: string
  username: string
  password: string
  listenPort: string
  realityDest: string
}

const PROTO_OPTIONS = ['socks5', 'http']
const field = 'mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm'

export default function UpstreamForm({
  values,
  onChange,
  editing,
  adding,
  onSubmit,
  onCancel,
}: {
  values: UpstreamFormValues
  onChange: <K extends keyof UpstreamFormValues>(key: K, value: UpstreamFormValues[K]) => void
  editing: boolean
  adding: boolean
  onSubmit: () => void
  onCancel: () => void
}) {
  return (
    <div className="mt-2 grid grid-cols-2 gap-2">
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
        上游协议
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
              findRealityDest(values.realityDest)?.warn ? 'text-amber-400' : 'text-neutral-500'
            }`}
          >
            {findRealityDest(values.realityDest)?.note}
          </p>
        )}
      </div>
      {values.proto === 'http' && (
        <p className="col-span-2 text-xs text-amber-400">
          HTTP 上游不支持 UDP（CONNECT 只能承载 TCP）——要 UDP 请选 socks5。
        </p>
      )}
      <div className="col-span-2 flex gap-2">
        <button
          type="button"
          disabled={adding}
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
