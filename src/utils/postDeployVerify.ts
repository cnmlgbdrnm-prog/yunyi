// 部署后自动验证：挑出「这次部署刚写回」的节点，交给测速任务去验。
//
// 关联方式是 addr === 服务器 host + source='self-built' —— deploy 与 upstream 写节点时
// 正是这么填的（addr = 服务器 host）。再按本次部署的协议收窄：同一台服务器上可能有历史
// 节点（别的协议、甚至别的用途），不该被一起拖去测，那会让「验证」变成一次全库测速。
//
// 协议清单为空时返回空——宁可什么都不测，也不要因为调用方忘了传参数而误测一批节点。
import type { Node } from '../api/types'

export function nodesToVerify(nodes: Node[], host: string, protocols: string[]): Node[] {
  if (protocols.length === 0) return []
  return nodes.filter(
    (n) => n.source === 'self-built' && n.addr === host && protocols.includes(n.protocol),
  )
}
