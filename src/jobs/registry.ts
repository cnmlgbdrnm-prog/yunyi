// 模块级任务注册表：后台任务的状态不再挂在视图组件上。
//
// 为什么必须是模块级：视图是条件挂载的（App.tsx:73-80），组件本地 state 切页即销毁，
// 而后端任务仍在跑——表现为「任务在跑但界面没有忙碌态、没有取消入口、可以重复提交
// 同一批远端部署」（评审 #5）。状态提到这里后，切走再切回，进行中的任务、它已经收到
// 的行内结果、以及取消入口都还在。
//
// 终态判定只在本文件做一处：**任务级**事件满足 `progress >= 100 || error != null` 即终态。
// 依据是 jobs/mod.rs:253-276 的补发终态事件——失败必带 error、成功必带 progress=100，
// 所以前端不必依赖「完成」「结束」这类文案（原先 Servers.tsx:377 与 LinkWizard.tsx:76
// 两套约定并存，后端改一个中文词就会静默失效）。
//
// **逐目标事件是例外**：message 命中本任务某个目标 id 的事件，error 是**那个目标**的
// 失败原因而不是任务终态（jobs/mod.rs:223-227 刻意保留给 speed/server_speed 传单台原因）。
// 这类事件只写 results、绝不判终态——否则一台机器失败就会把整个任务提前判死，
// 后面还没跑的机器再也不上屏。
import { useEffect, useMemo, useRef, useSyncExternalStore } from 'react'
import type { listen } from '@tauri-apps/api/event'
import { listJobs } from '../api'
import type { Job, JobProgress } from '../api/types'

/**
 * 任务种类。
 *
 * 「前端角色」与后端 kind 不是一对一：`server_throughput`（iperf 对测）与
 * `verify`（部署/上游写回后的自动验证）在后端分别复用 `server_speed` 与 `speed`。
 * 分开是因为终态处置不同（对测要另刷吞吐列表），且同一时刻可能真有两种任务在跑，
 * 混在一个桶里会把 A 的进度画到 B 的位置上。`peer_remove` 同理：加设备与删设备
 * 成功后的提示不同，而那句提示要能跨切页存活，就不能再靠组件里记「这次点的是哪个」。
 * `link_build` 更是必须分开：建链向导与拆链都发后端 `link` 事件，而两者的目标都是
 * 数字 id（拆链的是 rule id），混在一个桶里会认错任务、把拆链进度画进建链向导。
 */
export type JobKind =
  | 'deploy'
  | 'upstream'
  | 'privacy'
  | 'link'
  | 'peer'
  | 'speed'
  | 'server_speed'
  | 'server_throughput'
  | 'verify'
  | 'peer_remove'
  | 'link_build'

/**
 * 前端登记的 kind → 后端会用哪些 kind 发这个任务的事件。
 *
 * 为什么不直接比字符串：前端把「服务器批量测速」与「iperf3 对测」分成两个 kind
 * （两者终态处置不同——对测要多刷一次吞吐列表），而后端两个任务**共用一个 kind**
 * `server_speed`（jobs/mod.rs:109 的 `ServerSpeed | Throughput => "server_speed"`）。
 * 其余 kind 一一对应。
 *
 * 校验本身也有意义：job id 全局唯一，kind 对不上说明事件串了任务，
 * 宁可丢弃也不要让别的任务的进度写进这一行。
 */
const BACKEND_KINDS: Record<JobKind, readonly string[]> = {
  deploy: ['deploy'],
  upstream: ['upstream'],
  privacy: ['privacy'],
  link: ['link'],
  peer: ['peer'],
  speed: ['speed'],
  server_speed: ['server_speed'],
  server_throughput: ['server_speed'],
  verify: ['speed'],
  peer_remove: ['peer'],
  link_build: ['link'],
}

/** 某个目标在任务里的行内结果（后端约定：message=目标 id、error=该目标的失败原因）。 */
export interface TargetResult {
  error: string | null
}

export interface TrackedJob {
  /** 后端 job id；**负数 = IPC 还没返回时的本地占位**（见 reserveJob）。 */
  id: number
  kind: JobKind
  progress: number
  /** 最近一条任务级事件的步骤与文案（进度面板直接吃这两个字段） */
  step: string
  message: string
  error: string | null
  /** 本任务的目标（node id / server id 的字符串形式） */
  targets: string[]
  /** 逐目标结果。**按任务分桶**：一个任务终结只带走它自己的桶，
   *  别的任务还在跑的行内结果不受影响（评审 #7 原先终态 `setLiveResults({})` 全清）。 */
  results: ReadonlyMap<string, TargetResult>
}

/** 对账周期：事件流是主路径，定时查库只兜「终态事件丢了」这一种情况。 */
const RECONCILE_MS = 5_000
/** 占位存活上限：IPC 一直不回来（后端崩了/被杀）时不能让按钮永远停在忙碌态。 */
const RESERVE_TTL_MS = 60_000

const jobs = new Map<number, TrackedJob>()
/** 占位（负数 id）的登记时刻，用于超时清理；真实 job 不在此表。 */
const reservedAt = new Map<number, number>()
const listeners = new Set<() => void>()
const terminalListeners = new Set<(job: TrackedJob) => void>()
/** 引用稳定：useSyncExternalStore 靠它判定「变没变」，每次 emit 换一个新 Map。 */
let snapshot: ReadonlyMap<number, TrackedJob> = jobs
let nextTicket = -1

function emit(): void {
  snapshot = new Map(jobs)
  for (const fn of listeners) fn()
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn)
  return () => {
    listeners.delete(fn)
  }
}

export function getSnapshot(): ReadonlyMap<number, TrackedJob> {
  return snapshot
}

/** 终态通知：注册表是终态的唯一判据，视图据此刷新列表、上报终态错误（经 useJobTerminal）。 */
function subscribeTerminal(fn: (job: TrackedJob) => void): () => void {
  terminalListeners.add(fn)
  return () => {
    terminalListeners.delete(fn)
  }
}

/**
 * 乐观占位：**发 IPC 之前**调用，返回一个占位号（负数）。
 *
 * 这是 #6 的修法：原先忙碌态要等 `await` 回来才提交（Servers.tsx:225-234），
 * 那段时间里按钮既不禁用也没标记，连点两下就是两次远端任务。
 */
export function reserveJob(kind: JobKind, targets: string[]): number {
  const id = nextTicket--
  jobs.set(id, {
    id,
    kind,
    progress: 0,
    step: '',
    message: '',
    error: null,
    targets,
    results: new Map(),
  })
  reservedAt.set(id, Date.now())
  emit()
  return id
}

/** 占位转正：IPC 返回真实 job id 后就地改名。改名前后没有 await，不会漏事件。 */
export function bindJob(ticket: number, jobId: number): void {
  const cur = jobs.get(ticket)
  if (cur == null) return
  jobs.delete(ticket)
  reservedAt.delete(ticket)
  jobs.set(jobId, { ...cur, id: jobId })
  emit()
  // 「提交 → 登记」这段窗口里的终态事件会因 job id 不认识被丢掉：毫秒级失败
  // （参数非法、连不上）正好落在这里，不补查一次界面会一直停在「执行中」直到
  // 5s 对账。注册表自己补，各视图不必再各写一遍（原先 DeployWizard 与
  // UpstreamPanel 各有一份，LinkWizard 没有）。
  void reconcileJob(jobId)
}

/** 单任务终态对账（见 bindJob）。查不到/未终态都当无事发生。 */
async function reconcileJob(jobId: number): Promise<void> {
  let row: Job | undefined
  try {
    row = (await listJobs()).find((j) => j.id === jobId)
  } catch {
    return // 对账失败不理：周期对账仍会兜底
  }
  const cur = jobs.get(jobId)
  if (row == null || cur == null) return
  if (row.status !== 'done' && row.status !== 'failed' && row.status !== 'cancelled') return
  finish(cur, row.error, row.progress)
}

/** 撤下占位或放弃跟踪（IPC 失败、用户取消后本地复位）。 */
export function release(id: number): void {
  if (!jobs.delete(id)) return
  reservedAt.delete(id)
  emit()
}

function finish(job: TrackedJob, error: string | null, progress: number): void {
  jobs.delete(job.id)
  reservedAt.delete(job.id)
  emit()
  const final: TrackedJob = { ...job, error, progress }
  for (const fn of terminalListeners) fn(final)
}

/**
 * 事件入口：监听器与测试都走这里。
 *
 * 不是本会话发起的任务（注册表里没有）一律不接管——切页丢事件的问题由注册表本身解决，
 * 不靠「把后端所有任务都收进来」。
 */
export function __ingest(p: JobProgress): void {
  const cur = jobs.get(p.job_id)
  if (cur == null) return
  if (!BACKEND_KINDS[cur.kind].includes(p.kind)) return
  if (cur.targets.includes(p.message)) {
    // 逐目标明细：只写该目标的结果，**不动任务进度、不判终态**（见文件头注释）
    const results = new Map(cur.results)
    results.set(p.message, { error: p.error })
    jobs.set(p.job_id, { ...cur, results })
    emit()
    return
  }
  if (p.progress >= 100 || p.error != null) {
    finish(cur, p.error, p.progress)
    return
  }
  // 任务级进度：error 恒为 null（非 null 已在上面的终态分支返回）
  jobs.set(p.job_id, { ...cur, progress: p.progress, step: p.step, message: p.message })
  emit()
}

/** 测试专用：清空跟踪表与占位（不动已安装的监听）。 */
export function __reset(): void {
  jobs.clear()
  reservedAt.clear()
  nextTicket = -1
  emit()
}

/** 取在跟踪任务里的第一个：单任务面板用（同一时间只允许一个，如吞吐对测）。 */
export function firstJob(tracked: ReadonlyMap<number, TrackedJob>): TrackedJob | undefined {
  for (const job of tracked.values()) return job
  return undefined
}

/**
 * 进度面板的输入：**任务刚占位、还没有任何事件到达时给 null**，
 * 面板据此显示「任务已提交，等待执行…」占位（占位与「已收到事件」是两种界面状态，
 * 不能都渲染成「步骤：」+ 空文案）。
 */
export function progressView(
  job: TrackedJob | undefined,
): { step: string; message: string; progress: number } | null {
  if (job == null || job.step === '') return null
  return { step: job.step, message: job.message, progress: job.progress }
}

/** 覆盖该目标的在跟踪任务：忙态、进度面板、行内结果都从它取。 */
export function jobForTarget(
  tracked: ReadonlyMap<number, TrackedJob>,
  target: string,
): TrackedJob | undefined {
  for (const job of tracked.values()) {
    if (job.targets.includes(target)) return job
  }
  return undefined
}

/** 该目标是否已被某个在跟踪的任务占用（行内忙碌态与重复提交守卫共用同一判据）。 */
export function isTargetBusy(
  tracked: ReadonlyMap<number, TrackedJob>,
  target: string,
): boolean {
  return jobForTarget(tracked, target) != null
}

/** 该目标在在跟踪任务里的行内结果；没有任务覆盖它时返回 undefined。 */
export function targetResult(
  tracked: ReadonlyMap<number, TrackedJob>,
  target: string,
): TargetResult | undefined {
  return jobForTarget(tracked, target)?.results.get(target)
}

/** 可取消的后端任务号：占位（负数）后端不认，不能拿去 cancel_job。 */
export function cancelableJobIds(tracked: ReadonlyMap<number, TrackedJob>): number[] {
  const ids: number[] = []
  for (const job of tracked.values()) {
    if (job.id >= 0) ids.push(job.id)
  }
  return ids
}

/** 订阅在跟踪的任务；`kind` 省略时返回全部（对测与批量测速同属 server_speed 事件流）。 */
export function useTrackedJobs(kind?: JobKind): ReadonlyMap<number, TrackedJob> {
  const snap = useSyncExternalStore(subscribe, getSnapshot)
  return useMemo(() => {
    if (kind == null) return snap
    const out = new Map<number, TrackedJob>()
    for (const [id, job] of snap) {
      if (job.kind === kind) out.set(id, job)
    }
    return out
  }, [snap, kind])
}

/**
 * 终态回调。回调经 ref 引用：订阅只挂一次，视图每次渲染的新闭包不会重建订阅。
 * 视图被卸载时不订阅——但重挂载会重新拉一遍列表数据，这正是修掉 #5 的那条路。
 */
export function useJobTerminal(kind: JobKind, fn: (job: TrackedJob) => void): void {
  const ref = useRef(fn)
  ref.current = fn
  useEffect(
    () =>
      subscribeTerminal((job) => {
        if (job.kind === kind) ref.current(job)
      }),
    [kind],
  )
}

/** 监听器注入的签名（测试可传替身）。 */
export type ListenFn = typeof listen

let installed = false
/** 当前持有者数量。**必须是计数而不是布尔**，理由见 installJobListener。 */
let holders = 0
/** 已装配那一份的拆卸器；尚未装配完成为 null。 */
let teardown: (() => void) | null = null

/**
 * 安装全进程唯一的 `job://progress` 订阅，并启动唯一的对账定时器。
 *
 * **只装一次**：原先 7 个视图各注册一份（评审 #15），其中 DevicePeersPanel 与
 * LinkWizard 还没有对账兜底——丢一个终态事件就永久锁死面板。收拢到这里之后，
 * 这两个视图自动获得兜底，全进程也只剩一个定时器。
 *
 * **为什么持有者是计数而不是布尔**（2026-09-28 实测的线上 bug）：
 * `main.tsx` 挂着 `<StrictMode>`，开发构建下 React 按 setup → cleanup → setup 跑 effect，
 * 而 `listen()` 是异步的，于是真实时序是：
 * ```
 * setup1   holders=1，开始 await listen(...)
 * cleanup1 那时还没拿到卸载器（什么都不做）
 * setup2   —— 旧实现见「已装过」就返回 no-op
 * 这时 setup1 的 promise 才 resolve，它的卸载器被 cleanup1 的 disposed 分支调用
 * ```
 * 旧实现里那句卸载把监听器与定时器**双双拆掉**，而 setup2 手里是个 no-op、永不重装
 * ——此后进程内再无 job 事件、再无对账，**任何任务都永远停在忙碌态**：库里作业明明
 * 已经 `done`、测速结果也落了库，界面上的「测速中…」却再也不会消失。
 * 改成计数后，setup1 的卸载看到还有人持有就不拆，setup2 的卸载才是真正那次。
 *
 * 返回值是卸载函数；重复调用不会装第二份，但**每个调用方都要调用自己拿到的卸载函数**
 * （幂等，重复调用只生效一次）。
 */
export async function installJobListener(listenFn: ListenFn): Promise<() => void> {
  holders += 1
  if (!installed) {
    installed = true
    const unlisten = await listenFn('job://progress', (e) => {
      __ingest(e.payload as JobProgress)
    })
    const timer = setInterval(() => {
      void reconcile()
    }, RECONCILE_MS)
    teardown = () => {
      unlisten()
      clearInterval(timer)
      installed = false
      teardown = null
    }
    // 装配期间持有者可能已全部撤走（卸载早于 listen 落定）：这时没人会再来调
    // 卸载函数了，得就地拆掉，否则留下一份没人认领的监听器与定时器。
    if (holders === 0) teardown()
  }
  let released = false
  return () => {
    if (released) return
    released = true
    holders -= 1
    // 还有人拿着就**不许拆**：拆了对方手里的卸载器不会重装（见上）。
    if (holders > 0) return
    teardown?.()
  }
}

/**
 * 对账兜底：库里已终态、注册表里还在的任务清掉。
 *
 * 只查库不改库：事件流仍是主路径，这里补的是「终态事件没送到」（dev 重载、
 * 后端补发前进程被杀）。占位（负数 id）没有后端行，只按 TTL 清。
 */
async function reconcile(): Promise<void> {
  // 没有在跟踪的任务就不查库：省掉一次无谓 IPC（原先每个视图都无条件每 5s 查一次）
  if (jobs.size === 0) return
  let dirty = false
  const now = Date.now()
  for (const [id, at] of reservedAt) {
    if (now - at > RESERVE_TTL_MS) {
      jobs.delete(id)
      reservedAt.delete(id)
      dirty = true
    }
  }
  let rows: Job[]
  try {
    rows = await listJobs()
  } catch {
    // 对账失败不理：下一轮再试，事件流仍然有效
    if (dirty) emit()
    return
  }
  const finished: TrackedJob[] = []
  for (const [id, job] of jobs) {
    if (id < 0) continue
    const row = rows.find((j) => j.id === id)
    if (row == null) continue
    if (row.status !== 'done' && row.status !== 'failed' && row.status !== 'cancelled') continue
    jobs.delete(id)
    dirty = true
    finished.push({ ...job, error: row.error, progress: row.progress })
  }
  if (!dirty) return
  emit()
  for (const job of finished) {
    for (const fn of terminalListeners) fn(job)
  }
}
