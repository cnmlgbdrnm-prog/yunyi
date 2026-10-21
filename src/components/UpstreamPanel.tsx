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
import UpstreamForm, { type UpstreamFormValues } from './upstream/UpstreamForm'
import UpstreamRow from './upstream/UpstreamRow'
import { nodesToVerify } from '../utils/postDeployVerify'
import { DEFAULT_REALITY_DEST } from '../utils/realityDest'

const DEFAULT_LISTEN_PORT = '443'
const EMPTY_FORM: UpstreamFormValues = {
  name: '',
  proto: 'socks5',
  host: '',
  port: '',
  username: '',
  password: '',
  listenPort: DEFAULT_LISTEN_PORT,
  realityDest: DEFAULT_REALITY_DEST,
}

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
  const [form, setForm] = useState<UpstreamFormValues>(EMPTY_FORM)
  // 部署任务与「部署后自动验证」的进度。此前上游部署在 UI 上完全不可见：
  // 出口行一落库就显示，任务若失败只表现为「节点一直不出现」，没有第二处线索。
  // 两个任务都用本面板的 server id 作目标——同一页展开多个面板时不会互相串进度。
  const upstreamJobs = useTrackedJobs('upstream')
  const verifyJobs = useTrackedJobs('verify')
  const deployJob = jobForTarget(upstreamJobs, String(serverId))
  const verifyJob = jobForTarget(verifyJobs, String(serverId))
  const [verifyFailures, setVerifyFailures] = useState<string[]>([])

  const setField = <K extends keyof UpstreamFormValues>(key: K, value: UpstreamFormValues[K]) =>
    setForm((f) => ({ ...f, [key]: value }))

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

  const resetForm = () => {
    setEditingId(null)
    setForm(EMPTY_FORM)
  }

  const handleAdd = async () => {
    setError(null)
    setAdding(true)
    try {
      await addUpstream({
        serverId,
        name: form.name.trim(),
        proto: form.proto,
        host: form.host.trim(),
        port: Number(form.port),
        username: form.username.trim(),
        password: form.password,
        listenPort: Number(form.listenPort),
        realityDest: form.realityDest.trim() || undefined,
      })
      // 落库成功即触发部署：否则用户会看到一条「存在但服务器上还没有」的出口
      await watchJob(startUpstreamJob(serverId))
      resetForm()
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

  /** 行内改监听端口：其余字段按行上的原值原样带过去（**密码不带** = 不改）。
   *  后端撞车时会自动改用空闲端口，所以按返回值回显：与用户填的不同就说明被占了。 */
  const handleSavePort = (u: Upstream, raw: string) =>
    withBusy(u.id, async () => {
      const port = Number(raw)
      if (!Number.isInteger(port) || port < 1 || port > 65535) {
        setError('监听端口必须是 1-65535 之间的整数')
        return
      }
      const saved = await updateUpstream(u.id, {
        // 行内改端口只改端口：名称按原值完整带过去
        name: u.name,
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
          : `端口 ${port} 已被占用，已自动改用 ${saved.listen_port}，点「重新部署」后在远端生效`,
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
    setForm({
      name: u.name,
      proto: u.proto,
      host: u.host,
      port: String(u.port),
      username: u.username,
      password: '',
      listenPort: String(u.listen_port),
      realityDest: u.reality_dest,
    })
    setShowForm(true)
  }

  /** 保存编辑。**只写库**，远端要等「重新部署」——与行内改端口同一套语义 */
  const handleSaveEdit = () => {
    const id = editingId
    if (id == null) return
    return withBusy(id, async () => {
      const p = Number(form.port)
      const lp = Number(form.listenPort)
      if (!Number.isInteger(p) || p < 1 || p > 65535) {
        setError('上游端口必须是 1-65535 之间的整数')
        return
      }
      if (!Number.isInteger(lp) || lp < 1 || lp > 65535) {
        setError('监听端口必须是 1-65535 之间的整数')
        return
      }
      const saved = await updateUpstream(id, {
        name: form.name.trim(),
        proto: form.proto,
        host: form.host.trim(),
        port: p,
        username: form.username.trim(),
        // 留空 = 不改：凭据不回显，把空串发下去等于把已存口令擦掉
        password: form.password.trim() ? form.password : undefined,
        listenPort: lp,
        realityDest: form.realityDest.trim(),
      })
      setNotice(
        saved.listen_port === lp
          ? '改动已保存，点「重新部署」后在远端生效'
          : `监听端口 ${lp} 已被占用，已自动改用 ${saved.listen_port}，点「重新部署」后在远端生效`,
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
            <UpstreamRow
              key={u.id}
              u={u}
              editingValue={editing?.id === u.id ? editing.value : null}
              busy={busy.has(u.id)}
              stale={staleEdits.has(u.id)}
              onEditingValueChange={(value) => setEditing({ id: u.id, value })}
              onSavePort={() => {
                if (editing?.id === u.id) void handleSavePort(u, editing.value)
              }}
              onStartEditPort={() => setEditing({ id: u.id, value: String(u.listen_port) })}
              onCancelEditPort={() => setEditing(null)}
              onEdit={() => startEdit(u)}
              onProbe={() => void handleProbe(u.id)}
              onRedeploy={() => void handleRedeploy(u.id)}
              onRemove={() => void handleRemove(u.id)}
            />
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
        <UpstreamForm
          values={form}
          onChange={setField}
          editing={editingId != null}
          adding={adding}
          onSubmit={() => void (editingId != null ? handleSaveEdit() : handleAdd())}
          onCancel={() => {
            resetForm()
            setShowForm(false)
          }}
        />
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
