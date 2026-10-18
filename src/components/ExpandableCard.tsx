// 服务器卡片就地展开容器：受控 expanded，展开时渲染 children 并滚动到可见位置；
// 同一视口同时只应有一个展开态（由父组件互斥控制），收起由 onClose 通知父级。
import { useEffect, useRef } from 'react'

interface Props {
  /** 展开态由父级控制（互斥展开模型：同一时间最多一张卡片展开） */
  expanded: boolean
  /** 展开区根节点的稳定标识（DOM 断言/测试用） */
  id: string
  /** 卡片常驻头部（含操作按钮区） */
  header: React.ReactNode
  /** 展开内容（部署向导/编辑表单等） */
  children: React.ReactNode
  /** 收起回调：展开区内「收起」按钮触发 */
  onClose: () => void
}

export default function ExpandableCard({ expanded, id, header, children, onClose }: Props) {
  const panelRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    if (!expanded) return
    // 视口自动对准展开区顶部（nearest：只在需要时滚动，不打扰用户上下文）；
    // jsdom 未实现 scrollIntoView，须先判函数存在（测试环境跳过滚动）
    const el = panelRef.current
    if (el && typeof el.scrollIntoView === 'function') {
      el.scrollIntoView({ block: 'nearest', behavior: 'smooth' })
    }
  }, [expanded])

  return (
    <div data-expandable-card={id}>
      {header}
      {expanded && (
        <div ref={panelRef} className="mt-3">
          <div className="flex justify-end">
            <button
              type="button"
              aria-label={`收起展开区 ${id}`}
              onClick={onClose}
              className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-300 hover:bg-neutral-700"
            >
              收起
            </button>
          </div>
          {children}
        </div>
      )}
    </div>
  )
}
