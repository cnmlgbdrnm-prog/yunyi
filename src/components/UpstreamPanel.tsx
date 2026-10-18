// 上游出口面板：把买来的静态住宅 IP 代理（SOCKS5/HTTP，没有 SSH）接成这台中转机的落地。
// 提交 = 先落库（纯本地）再跑部署任务（在中转机上渲染专用 Xray 实例并写回节点），
// 所以「保存并部署」是一个动作、两个 IPC。出口列表与出口 IP 都是后端事实，出错红字携带真实原因。
// 部署任务与「部署后自动验证」的进度都取自模块级注册表（jobs/registry）：目标即本面板的
// server id，同一页上展开多个面板时各认各的任务。
import { useCallback, useEffect, useState } from 'react'
import {
  addUpstream,
  listNodes,
  listUpstreams,
  probeEgressIp,
  removeUpstream,
  speedTest,
  startUpstreamJob,
  updateUpstream,
} from '../api'
import type { Upstream } from '../api/types'
import { bindJob, jobForTarget, progressView, release, reserveJob, useJobTerminal, useTrackedJobs } from '../jobs/registry'
import JobProgressPanel from './JobProgressPanel'
import { nodesToVerify } from '../utils/postDeployVerify'
import { DEFAULT_REALITY_DEST, REALITY_DEST_GROUPS, findRealityDest } from '../utils/realityDest'

const PROTO_OPTIONS = ['socks5', 'http']
const DEFAULT_LISTEN_PORT = '443'

export default function UpstreamPanel({
  serverId,
  serverHost,
}: {
  serverId: number
  /** 部署后自动验证靠 addr === serverHost 认出「这台机器刚写回的节点」 */
  serverHost: string
}) {
  const [rows, setRows] = useState<Upstream[]>([])
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [busy, setBusy] = useState<Set<number>>(new Set())
  const [showForm, setShowForm] = useState(false)
  const [adding, setAdding] = useState(false)
  // 监听端口行内编辑：{出口 id, 当前输入}；null = 没在编辑
  const [editing, setEditing] = useState<{ id: number; value: string } | null>(null)
  // 整条出口的编辑：非 null 时下方那张表单是**编辑面**而不是新增面（同一套字段）
  const [editingId, setEditingId] = useState<number | null>(null)
  // 改过但还没重新部署的出口——远端仍是老配置，行上要留痕
  const [staleEdits, setStaleEdits] = useState<Set<number>>(new Set())
  // 表单：监听端口必填（默认 443），伪装域名可留空走默认
  const [name, setName] = useState('')
  const [proto, setProto] = useState('socks5')
  const [host, setHost] = useState('')
  const [port, setPort] = useState('')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [listenPort, setListenPort] = useState(DEFAULT_LISTEN_PORT)
  const [realityDest, setRealityDest] = useState(DEFAULT_REALITY_DEST)
  // 部署任务与「部署后自动验证」的进度。此前上游部署在 UI 上完全不可见：
  // 出口行一落库就显示，任务若失败只表现为「节点一直不出现」，没有第二处线索。
  // 两个任务都用本面板的 server id 作目标——同一页展开多个面板时不会互相串进度。
  const upstreamJobs = useTrackedJobs('upstream')
  const verifyJobs = useTrackedJobs('verify')
  const deployJob = jobForTarget(upstreamJobs, String(serverId))
  const verifyJob = jobForTarget(verifyJobs, String(serverId))
  const [verifyFailures, setVerifyFailures] = useState<string[]>([])

  const refresh = useCallback(async () => {
    try {
      setRows(await listUpstreams(serverId))
    } catch (e) {
      setError(String(e))
    }
  }, [serverId])

  useEffect(() => {
    void refresh()
  }, [refresh])

  /** 上游出口写回的节点协议恒为 vless（专用实例走 REALITY），据此筛出出口节点 */
  const runVerify = useCallback(async () => {
    const ticket = reserveJob('verify', [String(serverId)])
    try {
      const ids = nodesToVerify(await listNodes(), serverHost, ['vless']).map((n) => n.id)
      setVerifyFailures([])
      if (ids.length === 0) {
        release(ticket)
        return
      }
      bindJob(ticket, await speedTest(ids, 1))
    } catch (e) {
      release(ticket)
      setVerifyFailures([`无法启动自动验证：${String(e)}`])
    }
  }, [serverHost, serverId])

  /** 验证终态读回：本次送验的节点里谁还带着 last_check_error（节点协议恒为 vless，
   *  按 serverHost 重新筛一遍即可，不必记「刚才送了哪几个」——那份记忆切页就没） */
  const readVerifyResult = useCallback(async () => {
    try {
      const nodes = await listNodes()
      const sentIds = nodesToVerify(nodes, serverHost, ['vless']).map((n) => n.id)
      setVerifyFailures(
        nodes
          .filter((n) => sentIds.includes(n.id) && n.last_check_error)
          .map((n) => `${n.name}：${n.last_check_error}`),
      )
    } catch {
      /* 回读失败不阻断：终态已经显示在进度面板上 */
    }
  }, [serverHost])

  // 部署终态 → 刷新出口列表 + 自动验证刚写回的节点；验证终态 → 列出没连上的节点。
  // 事件丢失由注册表那个唯一的 5s 对账兜底。
  useJobTerminal('upstream', (job) => {
    if (job.error) {
      setError(job.error)
      return
    }
    void refresh()
    void runVerify()
  })
  useJobTerminal('verify', () => {
    void readVerifyResult()
  })

  /** 提交部署任务并登记：占位先行（IPC 未返回时进度区就已出现），失败撤占位。
   *  「提交→登记」窗口里的终态由注册表的 bindJob 自己补查，这里不必再对账一遍。
   */
  const watchJob = async (start: Promise<number>) => {
    setError(null)
    setVerifyFailures([])
    const ticket = reserveJob('upstream', [String(serverId)])
    try {
      bindJob(ticket, await start)
    } catch (e) {
      release(ticket)
      throw e
    }
  }

  const withBusy = async (id: number, fn: () => Promise<void>) => {
    setError(null)
    setBusy((prev) => new Set(prev).add(id))
    try {
      await fn()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  const handleAdd = async () => {
    setError(null)
    setAdding(true)
    try {
      await addUpstream({
        serverId,
        name: name.trim(),
        proto,
        host: host.trim(),
        port: Number(port),
        username: username.trim(),
        password,
        listenPort: Number(listenPort),
        realityDest: realityDest.trim() || undefined,
      })
      // 落库成功即触发部署：否则用户会看到一条「存在但服务器上还没有」的出口
      await watchJob(startUpstreamJob(serverId))
      setName('')
      setHost('')
      setPort('')
      setUsername('')
      setPassword('')
      setListenPort(DEFAULT_LISTEN_PORT)
      setShowForm(false)
      await refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setAdding(false)
    }
  }

  const handleRemove = (id: number) =>
    withBusy(id, async () => {
      await removeUpstream(id)
      // 远端配置要跟着重渲染，否则端口还开着而界面上已经没这条了
      await watchJob(startUpstreamJob(serverId))
      await refresh()
    })

  const handleRedeploy = (id: number) =>
    withBusy(id, async () => {
      await watchJob(startUpstreamJob(serverId))
      setStaleEdits((s) => {
        const next = new Set(s)
        next.delete(id)
        return next
      })
      await refresh()
    })

  /** 保存端口。后端撞车时会自动改用空闲端口，所以**按返回值回显**：
   *  与用户填的不同就说明被占了，得说清楚换成了哪个。 */
  /** 行内改监听端口：其余字段按行上的原值原样带过去（**密码不带** = 不改） */
  const handleSavePort = (u: Upstream, raw: string) =>
    withBusy(u.id, async () => {
      const port = Number(raw)
      if (!Number.isInteger(port) || port < 1 || port > 65535) {
        setError('监听端口必须是 1-65535 之间的整数')
        return
      }
      const saved = await updateUpstream(u.id, {
        proto: u.proto,
        host: u.host,
        port: u.port,
        username: u.username,
        listenPort: port,
        realityDest: u.reality_dest,
      })
      setEditing(null)
      setNotice(
        saved.listen_port === port
          ? `监听端口已改为 ${port}，点「重新部署」后在远端生效`
          : `端口 ${port} 已被占用，已自动改用 ${saved.listen_port}，点「重新部署」后在远端生效`
      )
      // 改了但还没重部署：远端还是老配置，行上要留痕（重部署后清掉）
      setStaleEdits((s) => new Set(s).add(u.id))
      await refresh()
    })

  /** 把表单切成「编辑这条出口」，字段按现有值预填（密码留空 = 不改） */
  const startEdit = (u: Upstream) => {
    setError(null)
    setNotice(null)
    setEditingId(u.id)
    setName(u.name)
    setProto(u.proto)
    setHost(u.host)
    setPort(String(u.port))
    setUsername(u.username)
    setPassword('')
    setListenPort(String(u.listen_port))
    setRealityDest(u.reality_dest)
    setShowForm(true)
  }

  const resetForm = () => {
    setEditingId(null)
    setName('')
    setProto('socks5')
    setHost('')
    setPort('')
    setUsername('')
    setPassword('')
    setListenPort(DEFAULT_LISTEN_PORT)
    setRealityDest(DEFAULT_REALITY_DEST)
  }

  /** 保存编辑。**只写库**，远端要等「重新部署」——与行内改端口同一套语义 */
  const handleSaveEdit = () => {
    const id = editingId
    if (id == null) return
    return withBusy(id, async () => {
      const p = Number(port)
      const lp = Number(listenPort)
      if (!Number.isInteger(p) || p < 1 || p > 65535) {
        setError('上游端口必须是 1-65535 之间的整数')
        return
      }
      if (!Number.isInteger(lp) || lp < 1 || lp > 65535) {
        setError('监听端口必须是 1-65535 之间的整数')
        return
      }
      const saved = await updateUpstream(id, {
        proto,
        host: host.trim(),
        port: p,
        username: username.trim(),
        // 留空 = 不改：凭据不回显，把空串发下去等于把已存口令擦掉
        password: password.trim() ? password : undefined,
        listenPort: lp,
        realityDest: realityDest.trim(),
      })
      setNotice(
        saved.listen_port === lp
          ? '改动已保存，点「重新部署」后在远端生效'
          : `监听端口 ${lp} 已被占用，已自动改用 ${saved.listen_port}，点「重新部署」后在远端生效`
      )
      setEditingId(null)
      setShowForm(false)
      resetForm()
      setStaleEdits((s) => new Set(s).add(id))
      await refresh()
    })
  }

  const handleProbe = (id: number) =>
    withBusy(id, async () => {
      await probeEgressIp(id)
      await refresh()
    })

  const field = 'mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm'

  return (
    <div className="mt-2 rounded border border-neutral-800 bg-neutral-900/60 p-3">
      <p className="text-xs font-medium text-neutral-400">
        上游出口（住宅代理：客户端 → 中转机 → 住宅 IP）
      </p>
      {error && (
        <p role="alert" className="mt-1 text-xs text-red-400">
          {error}
        </p>
      )}
      {notice && (
        <p role="status" className="mt-1 text-xs text-neutral-400">
          {notice}
        </p>
      )}
      {rows.length === 0 && (
        <p className="mt-1 text-xs text-neutral-500">
          还没有上游出口。添加后会在这台中转机上部署一个专用 Xray 实例，并写回一条普通节点。
        </p>
      )}
      {rows.length > 0 && (
        <ul className="mt-2 space-y-1">
          {rows.map((u) => (
            <li key={u.id} className="rounded bg-neutral-900 px-2 py-1.5">
              <div className="flex items-center justify-between gap-2">
                <p className="truncate text-sm">
                  {u.name}{' '}
                  <span className="text-xs text-neutral-500">
                    {u.proto} {u.host}:{u.port} ·{' '}
                    {editing?.id === u.id ? (
                      <>
                        监听{' '}
                        <input
                          aria-label="监听端口"
                          className="w-20 rounded border border-neutral-700 bg-neutral-900 px-1 py-0.5 text-xs"
                          value={editing.value}
                          onChange={(e) =>
                            setEditing({ id: u.id, value: e.target.value })
                          }
                          onKeyDown={(e) => {
                            if (e.key === 'Enter') void handleSavePort(u, editing.value)
                            if (e.key === 'Escape') setEditing(null)
                          }}
                        />
                        <button
                          type="button"
                          disabled={busy.has(u.id)}
                          onClick={() => void handleSavePort(u, editing.value)}
                          className="ml-1 rounded bg-neutral-800 px-1.5 py-0.5 text-emerald-300 hover:bg-neutral-700 disabled:opacity-50"
                        >
                          保存
                        </button>
                        <button
                          type="button"
                          onClick={() => setEditing(null)}
                          className="ml-1 text-neutral-400 hover:text-neutral-200"
                        >
                          取消
                        </button>
                      </>
                    ) : (
                      <>
                        监听{' '}
                        <button
                          type="button"
                          aria-label={`改监听端口 ${u.listen_port}`}
                          onClick={() =>
                            setEditing({ id: u.id, value: String(u.listen_port) })
                          }
                          className="underline decoration-dotted hover:text-neutral-300"
                        >
                          {u.listen_port}
                        </button>
                        {staleEdits.has(u.id) && (
                          <span className="ml-1 text-amber-400">
                            （已改，重新部署后生效）
                          </span>
                        )}
                      </>
                    )}
                  </span>
                </p>
                <div className="flex shrink-0 items-center gap-2">
                  <button
                    type="button"
                    aria-label={`编辑 ${u.name}`}
                    disabled={busy.has(u.id)}
                    onClick={() => startEdit(u)}
                    className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-200 hover:bg-neutral-700 disabled:opacity-50"
                  >
                    编辑
                  </button>
                  <button
                    type="button"
                    disabled={busy.has(u.id)}
                    onClick={() => void handleProbe(u.id)}
                    className="rounded bg-neutral-800 px-2 py-1 text-xs text-emerald-300 hover:bg-neutral-700 disabled:opacity-50"
                  >
                    探测出口 IP
                  </button>
                  <button
                    type="button"
                    disabled={busy.has(u.id)}
                    onClick={() => void handleRedeploy(u.id)}
                    className="rounded bg-neutral-800 px-2 py-1 text-xs text-sky-300 hover:bg-neutral-700 disabled:opacity-50"
                  >
                    重新部署
                  </button>
                  <button
                    type="button"
                    disabled={busy.has(u.id)}
                    onClick={() => void handleRemove(u.id)}
                    className="rounded bg-neutral-800 px-2 py-1 text-xs text-red-400 hover:bg-neutral-700 disabled:opacity-50"
                  >
                    删除
                  </button>
                </div>
              </div>
              {u.egress_ip && (
                <p className="mt-0.5 text-xs text-emerald-400">出口 IP {u.egress_ip}</p>
              )}
              {u.last_check_error && (
                <p className="mt-0.5 text-xs text-red-400">{u.last_check_error}</p>
              )}
            </li>
          ))}
        </ul>
      )}

      <div className="mt-2">
        <button
          type="button"
          onClick={() => {
            // 从编辑态切回新增：字段要清干净，否则会把上一条的值带进新出口
            if (editingId != null) resetForm()
            setShowForm((v) => !v)
          }}
          className="rounded bg-neutral-800 px-3 py-1 text-xs text-sky-300 hover:bg-neutral-700"
        >
          {editingId != null ? '放弃编辑' : '添加出口'}
        </button>
      </div>

      {showForm && (
        <div className="mt-2 grid grid-cols-2 gap-2">
          <label className="text-xs text-neutral-400">
            出口名称
            <input
              aria-label="出口名称"
              className={field}
              value={name}
              // 名称参与写回节点的命名（<服务器>-<出口名>），改名要连带改远端节点，
              // 不在这次编辑面内——只读展示，免得用户以为改了却没生效
              disabled={editingId != null}
              onChange={(e) => setName(e.target.value)}
            />
            {editingId != null && (
              <span className="text-neutral-500">名称暂不可改（改名要连带改远端节点）</span>
            )}
          </label>
          <label className="text-xs text-neutral-400">
            上游协议
            <select
              aria-label="上游协议"
              className={field}
              value={proto}
              onChange={(e) => setProto(e.target.value)}
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
            <input aria-label="上游地址" className={field} value={host} onChange={(e) => setHost(e.target.value)} />
          </label>
          <label className="text-xs text-neutral-400">
            上游端口
            <input aria-label="上游端口" className={field} value={port} onChange={(e) => setPort(e.target.value)} />
          </label>
          <label className="text-xs text-neutral-400">
            用户名
            <input aria-label="用户名" className={field} value={username} onChange={(e) => setUsername(e.target.value)} />
          </label>
          <label className="text-xs text-neutral-400">
            密码
            <input
              aria-label="密码"
              type="password"
              className={field}
              value={password}
              placeholder={editingId != null ? '留空＝不改' : ''}
              onChange={(e) => setPassword(e.target.value)}
            />
          </label>
          <label className="text-xs text-neutral-400">
            监听端口
            <input
              aria-label="监听端口"
              className={field}
              value={listenPort}
              onChange={(e) => setListenPort(e.target.value)}
            />
          </label>
          <div className="col-span-2 text-xs text-neutral-400">
            伪装域名（专用实例的 REALITY 镜像目标，留默认即可）
            {/* 下拉只是「快捷填入」：输入框始终是事实源，两者都改得 */}
            <select
              aria-label="伪装域名预设"
              className={field}
              /* 手填了清单外的域名时回落到占位项，避免下拉显示与输入框不一致 */
              value={findRealityDest(realityDest) ? realityDest.trim() : ''}
              onChange={(e) => {
                if (e.target.value !== '') setRealityDest(e.target.value)
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
              value={realityDest}
              onChange={(e) => setRealityDest(e.target.value)}
            />
            {/* 选中项的标注：下拉里每个域名都带了实测结论，这里把当前这条展开说清楚 */}
            {findRealityDest(realityDest)?.note && (
              <p
                className={`mt-1 ${
                  findRealityDest(realityDest)?.warn ? 'text-amber-400' : 'text-neutral-500'
                }`}
              >
                {findRealityDest(realityDest)?.note}
              </p>
            )}
          </div>
          {proto === 'http' && (
            <p className="col-span-2 text-xs text-amber-400">
              HTTP 上游不支持 UDP（CONNECT 只能承载 TCP）——要 UDP 请选 socks5。
            </p>
          )}
          <div className="col-span-2 flex gap-2">
            <button
              type="button"
              disabled={adding}
              onClick={() => void (editingId != null ? handleSaveEdit() : handleAdd())}
              className="rounded bg-sky-800 px-3 py-1 text-xs text-sky-100 hover:bg-sky-700 disabled:opacity-50"
            >
              {editingId != null
                ? '保存改动'
                : adding
                  ? '部署中…'
                  : '保存并部署'}
            </button>
            {editingId != null && (
              <button
                type="button"
                onClick={() => {
                  resetForm()
                  setShowForm(false)
                }}
                className="rounded bg-neutral-800 px-3 py-1 text-xs text-neutral-300 hover:bg-neutral-700"
              >
                取消
              </button>
            )}
          </div>
        </div>
      )}

      {/* 部署进度：以前上游部署在界面上完全不可见——出口行一落库就显示，
          任务失败只表现为「节点一直不出现」，没有第二处线索 */}
      {deployJob != null && (
        <div className="mt-2 space-y-1 rounded border border-neutral-800 p-2">
          <p className="text-xs text-neutral-300">上游部署</p>
          <JobProgressPanel progress={progressView(deployJob)} />
        </div>
      )}

      {/* 部署后自动验证：部署成功 ≠ 节点连得上。
          任务终结后注册表会把它移除，但**结论要留在屏幕上**——失败清单就是这一步的
          全部价值，不能随任务一起消失（这也是原先任务态永不清空的那个副作用里
          唯一值得保留的部分）。 */}
      {(verifyJob != null || verifyFailures.length > 0) && (
        <div className="mt-2 space-y-1 rounded border border-neutral-800 p-2">
          <p className="text-xs text-neutral-300">自动验证：对写回的出口节点测速</p>
          <JobProgressPanel progress={progressView(verifyJob)} />
          {verifyFailures.length > 0 && (
            <div role="alert" className="text-xs text-red-400">
              <p>以下节点没连上：</p>
              <ul className="ml-4 list-disc">
                {verifyFailures.map((f) => (
                  <li key={f}>{f}</li>
                ))}
              </ul>
            </div>
          )}
        </div>
      )}
    </div>
  )
}
