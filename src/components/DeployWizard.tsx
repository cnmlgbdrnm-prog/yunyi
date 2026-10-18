// 部署向导（多协议）：协议卡片网格（L1 默认全选，L2 可选勾选）→ 提交 protocols 列表 →
// 就地展示分步进度与完成/失败状态。任务态取自模块级注册表（jobs/registry）：
// 目标即本向导的 server id；「提交→登记」窗口里的终态（毫秒级失败）由注册表补查。
// 取消经 cancelJob 通知后端后回调 onClose。
import { useCallback, useEffect, useRef, useState } from 'react'
import { cancelJob, changeRealityDest, listNodes, speedTest, startDeploy } from '../api'
import type { DeployProtocol, Server } from '../api/types'
import { bindJob, jobForTarget, progressView, release, reserveJob, useJobTerminal, useTrackedJobs } from '../jobs/registry'
import JobProgressPanel from './JobProgressPanel'
import { nodesToVerify } from '../utils/postDeployVerify'
import { DEFAULT_REALITY_DEST, REALITY_DEST_GROUPS, findRealityDest } from '../utils/realityDest'

interface Props {
  server: Server
  onClose: () => void
  /** 终态回调：携带本次提交的协议清单（卡片已部署协议徽章用） */
  onDone: (protocols: DeployProtocol[]) => void
}

/** 协议元信息（与后端 PROTOCOLS 白名单一致；l1 组默认全选） */
const PROTOCOLS_UI: { id: DeployProtocol; label: string; tag: string; l1: boolean }[] = [
  { id: 'vless', label: 'VLESS', tag: 'Reality · 抗检测主力', l1: true },
  { id: 'trojan', label: 'Trojan', tag: 'Reality · 双保险', l1: true },
  { id: 'vmess', label: 'VMess', tag: 'WS · 老客户端兼容', l1: true },
  { id: 'ss', label: 'SS-2022', tag: 'blake3 · 兜底稳定', l1: true },
  { id: 'hysteria2', label: 'Hysteria2', tag: 'UDP · 可选装', l1: false },
  { id: 'tuic', label: 'TUIC', tag: 'UDP · 可选装', l1: false },
]

/** 上次勾选（按服务器记忆，localStorage）：打开向导不再回到默认 L1——
 * 用户勾过 H2/TUIC 后再次打开应保留；无记录/数据损坏回默认 L1。
 */
function initSelected(serverId: number): Set<DeployProtocol> {
  try {
    const raw = localStorage.getItem(`deploy-selected-${serverId}`)
    if (raw) {
      const ids = JSON.parse(raw) as DeployProtocol[]
      if (Array.isArray(ids)) {
        const valid = PROTOCOLS_UI.map((p) => p.id)
        const arr = ids.filter((x) => valid.includes(x))
        if (arr.length > 0) return new Set(arr)
      }
    }
  } catch {
    /* 本地存储异常 → 默认 */
  }
  return new Set(PROTOCOLS_UI.filter((p) => p.l1).map((p) => p.id))
}

export default function DeployWizard({ server, onClose, onDone }: Props) {
  const [selected, setSelected] = useState<Set<DeployProtocol>>(
    () => initSelected(server.id),
  )
  const [realityDest, setRealityDest] = useState(DEFAULT_REALITY_DEST)
  // BBR 默认勾选、不持久化：BBR 幂等，已开启的机器再勾也只是检测后跳过
  const [enableBbr, setEnableBbr] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [starting, setStarting] = useState(false)
  const [changingDest, setChangingDest] = useState(false)
  // 部署成功的终态标记：那时任务已从注册表移除，向导要停在「完成」态
  // （只剩关闭），而不是又摆出「开始部署」勾人去重复部署一遍
  const [finished, setFinished] = useState(false)
  // 部署与「部署后自动验证」两个任务，都认本向导的 server id（同一页可能开多个向导）
  const deployJobs = useTrackedJobs('deploy')
  const verifyJobs = useTrackedJobs('verify')
  const deployJob = jobForTarget(deployJobs, String(server.id))
  const verifyJob = jobForTarget(verifyJobs, String(server.id))
  const deployProgress = progressView(deployJob)
  const [verifyFailures, setVerifyFailures] = useState<string[]>([])
  // 提交时定格协议清单（运行中复选框已禁用，与 selected 一致）：终态回调携带
  const requestedRef = useRef<DeployProtocol[]>([])

  // 部署完成后自动验证：把刚写回的节点送去测速。
  // 不做这步的话，「部署成功」和「节点真的连得上」之间没有任何反馈——
  // 这次排查的两条 REALITY 节点就是这么在库里躺着的：界面显示部署成功，实际连不上，
  // 而库里除了「节点存在」之外没有任何线索。
  const runVerify = async (protocols: DeployProtocol[]) => {
    let ids: string[]
    try {
      ids = nodesToVerify(await listNodes(), server.host, protocols).map((n) => n.id)
    } catch (e) {
      setVerifyFailures([`无法启动自动验证：${String(e)}`])
      return
    }
    setVerifyFailures([])
    if (ids.length === 0) return
    // targets = [认领用的 server id, ...本次送验的节点]：终态回读直接按节点 id 定位，
    // 不重算筛选条件——那份条件（protocols）是组件状态，切页重挂后就没了，重算会选错范围
    const ticket = reserveJob('verify', [String(server.id), ...ids])
    try {
      bindJob(ticket, await speedTest(ids, 1))
    } catch (e) {
      release(ticket)
      setVerifyFailures([`无法启动自动验证：${String(e)}`])
    }
  }

  // 部署终态：成功回调 onDone（携带协议清单）+ 送验；失败只上屏原因。
  // 事件丢失由注册表那个唯一的 5s 对账兜底。
  useJobTerminal('deploy', (job) => {
    if (job.error) {
      setError(job.error)
      return
    }
    setFinished(true)
    onDone(requestedRef.current)
    void runVerify(requestedRef.current)
  })
  useJobTerminal('verify', (job) => {
    // targets[0] 是认领用的 server id，其余是本次送验的节点
    const ids = job.targets.slice(1)
    void (async () => {
      try {
        setVerifyFailures(
          (await listNodes())
            .filter((n) => ids.includes(n.id) && n.last_check_error)
            .map((n) => `${n.name}：${n.last_check_error}`),
        )
      } catch {
        /* 回读失败不阻断：任务终态已经显示在进度面板上 */
      }
    })()
  })

  const toggle = (id: DeployProtocol) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(id)) {
        next.delete(id)
      } else {
        next.add(id)
      }
      return next
    })
  }

  // 勾选变化即持久化（下次打开同服务器恢复上次选择）
  useEffect(() => {
    try {
      localStorage.setItem(`deploy-selected-${server.id}`, JSON.stringify([...selected]))
    } catch {
      /* 存储失败不影响操作 */
    }
  }, [selected, server.id])

  const hasReality = selected.has('vless') || selected.has('trojan')
  // 当前填的域名在预设清单里的标注（手填的清单外域名 → undefined，不显示任何结论）
  const selectedDest = findRealityDest(realityDest)

  const handleStart = useCallback(async () => {
    setError(null)
    const protocols = [...selected]
    if (protocols.length === 0) {
      setError('未选择任何协议')
      return
    }
    setStarting(true)
    setFinished(false)
    requestedRef.current = protocols
    // 占位先行：IPC 回来之前进度区就已出现「准备中…」；「提交→登记」窗口里的
    // 终态（毫秒级失败）由注册表 bindJob 自己补查，这里不必再对账一遍
    const ticket = reserveJob('deploy', [String(server.id)])
    try {
      bindJob(
        ticket,
        await startDeploy({
          serverId: server.id,
          protocols,
          realityDest: hasReality ? realityDest : undefined,
          enableBbr,
        }),
      )
    } catch (e) {
      release(ticket)
      setError(String(e))
    } finally {
      setStarting(false)
    }
  }, [server.id, selected, realityDest, hasReality, enableBbr])

  // 仅更换伪装域名（轻量）：服务器 xray 就地改写+重启+回写节点 sni，秒级，不重部署
  const handleChangeDest = async () => {
    setError(null)
    const dest = realityDest.trim()
    if (!dest) {
      setError('请输入伪装域名')
      return
    }
    setChangingDest(true)
    try {
      await changeRealityDest(server.id, dest)
      onClose()
    } catch (e) {
      setError(String(e))
    } finally {
      setChangingDest(false)
    }
  }

  const handleCancel = async () => {
    const job = deployJob
    if (job != null) {
      if (job.id >= 0) {
        try {
          await cancelJob(job.id)
        } catch (e) {
          setError(String(e))
        }
      }
      // 本地先撤（后端终态事件到达时任务已不在表里，自然忽略）；占位（负数 id）后端不认
      release(job.id)
    }
    onClose()
  }

  const failed = error != null
  // 任务还在跟踪表里 = 还在跑：终态即被注册表移除，不必再自己判 progress>=100
  const running = deployJob != null && !failed

  return (
    <div className="space-y-3">
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}
      <div>
        <p className="mb-1.5 text-sm text-neutral-300">
          选协议（点卡片勾选，L1 必装组默认全选；Hysteria2/TUIC 装前自动 UDP 通路探测）
        </p>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
          {PROTOCOLS_UI.map((p) => (
            <label
              key={p.id}
              className={`flex cursor-pointer select-none items-start gap-2 rounded border p-2 text-sm ${
                selected.has(p.id)
                  ? 'border-emerald-700 bg-emerald-950/40'
                  : 'border-neutral-700 bg-neutral-900 hover:border-neutral-500'
              }`}
            >
              <input
                type="checkbox"
                name={`协议 ${p.label}`}
                aria-label={p.label}
                checked={selected.has(p.id)}
                disabled={running}
                onChange={() => toggle(p.id)}
                className="mt-0.5 accent-emerald-600"
              />
              <span className="leading-tight">
                <span className="block font-medium text-neutral-100">{p.label}</span>
                <span className="block text-xs text-neutral-400">{p.tag}</span>
              </span>
            </label>
          ))}
        </div>
      </div>

      <label className="flex cursor-pointer select-none items-center gap-2 text-sm text-neutral-300">
        <input
          type="checkbox"
          aria-label="同时开启 BBR"
          checked={enableBbr}
          disabled={running}
          onChange={(e) => setEnableBbr(e.target.checked)}
          className="accent-emerald-600"
        />
        同时开启 BBR（拥塞控制，已开启的机器自动跳过）
      </label>

      {hasReality && !running && (
        <div className="block text-sm">
          {/* 下拉只是「快捷填入」：输入框始终是事实源，两者都改得 */}
          <label className="block">
            常用伪装域名（下拉点选自动填入，也可直接在下面手填）
            <select
              aria-label="Reality 伪装域名预设"
              className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
              /* 手填了清单外的域名时回落到占位项，避免下拉显示与输入框不一致 */
              value={findRealityDest(realityDest) ? realityDest.trim() : ''}
              disabled={changingDest}
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
          </label>
          <label className="mt-1 block">
            Reality 伪装域名
            <input
              aria-label="Reality 伪装域名"
              className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
              value={realityDest}
              disabled={changingDest}
              onChange={(e) => setRealityDest(e.target.value)}
            />
          </label>
          {/* 选中项的标注：下拉里每个域名都带了实测结论，这里把当前这条展开说清楚 */}
          {selectedDest?.note && (
            <p className={`mt-1 text-xs ${selectedDest.warn ? 'text-amber-400' : 'text-neutral-500'}`}>
              {selectedDest.note}
            </p>
          )}
          <button
            type="button"
            onClick={() => void handleChangeDest()}
            disabled={changingDest}
            title="仅改服务器 xray 的 dest 并重启（约 2 秒），不重新部署其它配置"
            className="mt-1 rounded bg-neutral-800 px-2 py-1 text-xs text-sky-300 hover:bg-neutral-700 disabled:opacity-50"
          >
            {changingDest ? '更换中…' : '仅更换伪装域名（不重部署）'}
          </button>
        </div>
      )}

      {deployJob != null && (
        <div className="space-y-1 rounded border border-neutral-800 p-3">
          {deployProgress ? (
            <>
              <p className="text-sm text-neutral-300">
                步骤：<span>{deployProgress.step}</span>
              </p>
              {deployProgress.message !== '' && <p className="text-sm text-neutral-400">{deployProgress.message}</p>}
              <div className="h-2 w-full overflow-hidden rounded bg-neutral-800">
                <div className="h-full bg-emerald-600 transition-all" style={{ width: `${Math.min(100, Math.max(0, deployProgress.progress))}%` }} />
              </div>
            </>
          ) : (
            <p className="text-sm text-neutral-300">准备中…</p>
          )}
        </div>
      )}

      {/* 部署后自动验证：部署成功 ≠ 节点连得上，这一步把差距当场摆出来。
          任务终结后注册表会移除它，但**结论要留在屏幕上**（失败清单就是这一步的全部价值）。 */}
      {(verifyJob != null || verifyFailures.length > 0) && (
        <div className="space-y-1 rounded border border-neutral-800 p-3">
          <p className="text-sm text-neutral-300">
            自动验证：对本次部署写入的节点测速
          </p>
          <JobProgressPanel progress={progressView(verifyJob)} />
          {verifyFailures.length > 0 && (
            <div role="alert" className="text-sm text-red-300">
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

      <div className="flex gap-2">
        {running ? (
          <button type="button" onClick={handleCancel} className="rounded bg-red-800 px-3 py-1.5 text-sm text-white hover:bg-red-700">
            取消
          </button>
        ) : failed || finished ? (
          <button type="button" onClick={onClose} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
            关闭
          </button>
        ) : (
          <>
            <button
              type="button"
              onClick={handleStart}
              disabled={starting}
              className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50"
            >
              {starting ? '准备中…' : '开始部署'}
            </button>
            <button type="button" onClick={onClose} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
              关闭
            </button>
          </>
        )}
      </div>
    </div>
  )
}
