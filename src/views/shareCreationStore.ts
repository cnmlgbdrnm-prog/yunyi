// 分享创建任务的前端状态：**必须活在模块级**。
//
// 视图是条件挂载的（App.tsx），生成链接要 SSH 改远端配置，耗时以秒计；状态若挂在
// ShareWizard 的本地 state 上，用户切走再回来就只剩一个空列表——任务其实还在跑，
// 既看不到忙碌态、也能再点一次发起重复创建。这与 jobs/registry.ts 修的是同一类
// 问题，只是 create_share_cmd 不是后端 job，没有 job://progress 可订阅。
//
// 成功结果同时并入 shareStatusStore：即使用户当时不在分享页，切回来列表里也有
// 这条新分享，不必等下一轮 120s 轮询，也不必额外跑一次全量 SSH。
import { useSyncExternalStore } from 'react'
import { createShare, setSetting } from '../api'
import type { ShareInfo } from '../api/types'
import { upsertShare } from './shareStatusStore'

export interface ShareCreationInput {
  serverId: number
  serverName: string
  nodeIds: string[]
  quotaTotalGb: number
  rateUpMbps: number
  rateDownMbps: number
  expiresInDays: number
  cdn?: { domain: string; port: number } | null
}

export interface ShareCreation {
  serverId: number
  serverName: string
  status: 'running' | 'done' | 'error'
  result: ShareInfo | null
  error: string | null
}

let state: ShareCreation | null = null
const listeners = new Set<() => void>()

function emit(): void {
  for (const fn of listeners) fn()
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn)
  return () => {
    listeners.delete(fn)
  }
}

function getSnapshot(): ShareCreation | null {
  return state
}

export function useShareCreation(): ShareCreation | null {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot)
}

/** 测试用：读当前状态。 */
export function getShareCreation(): ShareCreation | null {
  return state
}

/**
 * 发起一次创建。同一时间只允许一个任务：第二次调用直接拒绝，避免切页后重复提交
 * 在远端生成两条分享。Promise 由 store 持有——组件卸载不会取消它，结果仍会落到
 * state 里供切回时展示。
 */
export async function runShareCreation(input: ShareCreationInput): Promise<ShareInfo> {
  if (state?.status === 'running') {
    throw new Error(`已有分享正在生成（${state.serverName}），请等待完成`)
  }
  state = {
    serverId: input.serverId,
    serverName: input.serverName,
    status: 'running',
    result: null,
    error: null,
  }
  emit()
  try {
    if (input.cdn !== undefined) {
      await setSetting(`share_cdn_${input.serverId}`, JSON.stringify(input.cdn))
    }
    const result = await createShare(
      input.serverId,
      input.nodeIds,
      input.rateUpMbps,
      input.rateDownMbps,
      input.expiresInDays,
      input.quotaTotalGb,
    )
    upsertShare(result)
    state = {
      serverId: input.serverId,
      serverName: input.serverName,
      status: 'done',
      result,
      error: null,
    }
    emit()
    return result
  } catch (e) {
    state = {
      serverId: input.serverId,
      serverName: input.serverName,
      status: 'error',
      result: null,
      error: String(e),
    }
    emit()
    throw e
  }
}

/** 关闭成功/失败提示。运行中的任务不能关掉——关掉就又没有进度入口了。 */
export function dismissShareCreation(): void {
  if (state?.status === 'running') return
  state = null
  emit()
}

/** 测试专用：清空状态。 */
export function __resetShareCreation(): void {
  state = null
  emit()
}
