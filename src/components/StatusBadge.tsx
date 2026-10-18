// 链路状态徽标：纯文字 + 配色表达状态（UI 规范禁用 emoji）。
// 输入：RelayRule.status 字符串；输出：带语义配色的圆角标签。
// 未知状态兜底为灰色，不因后端新增状态而崩溃。
const STATUS_STYLE: Record<string, string> = {
  built: 'bg-emerald-950 text-emerald-300',
  building: 'bg-yellow-950 text-yellow-300',
  imported: 'bg-blue-950 text-blue-300',
  failed: 'bg-red-950 text-red-300',
  'not-built': 'bg-neutral-800 text-neutral-400',
}

const STATUS_LABEL: Record<string, string> = {
  built: '已建链',
  building: '建链中',
  imported: '已接管',
  failed: '失败',
  'not-built': '未建链',
}

export default function StatusBadge({ status }: { status: string }) {
  return (
    <span className={`inline-block rounded px-2 py-0.5 text-xs ${STATUS_STYLE[status] ?? STATUS_STYLE['not-built']}`}>
      {STATUS_LABEL[status] ?? status}
    </span>
  )
}
