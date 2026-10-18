// 地址族切换：一次点击要改服务器与全部自建直连节点（后端同一事务），再热应用配置。
// 忙状态按服务器 id 分桶，切完重读服务器与部署徽章；错误统一交给调用方的上屏函数。
import { useCallback, useState } from 'react'
import { setServerAddrFamily } from '../../api'

export function useAddrFamilySwitch(
  reportError: (message: string | null) => void,
  refresh: () => Promise<void>,
  loadDeployed: (ids: number[]) => Promise<void>,
) {
  const [busy, setBusy] = useState<Set<number>>(new Set())
  const switchTo = useCallback(
    async (id: number, family: 'v4' | 'v6') => {
      reportError(null)
      setBusy((prev) => new Set(prev).add(id))
      try {
        await setServerAddrFamily(id, family)
        await refresh()
        await loadDeployed([id])
      } catch (e) {
        reportError(String(e))
      } finally {
        setBusy((prev) => {
          const next = new Set(prev)
          next.delete(id)
          return next
        })
      }
    },
    [reportError, refresh, loadDeployed],
  )
  return { busy, switchTo }
}
