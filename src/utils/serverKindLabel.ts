// 服务器类型（Server.kind）→ 展示标签。三处都要用（服务器行、对测的源/目标下拉、
// 卡片标题），各写一份的话改了一边漏另一边只是时间问题。
//
// 未知类型**原样返回**而不是空串：后端加了新 kind 时，界面该显示那个原始值让人看出
// 「有东西不认识」，而不是留一片空白。
export function serverKindLabel(kind: string): string {
  switch (kind) {
    case 'relay':
      return '中转'
    case 'landing':
      return '落地'
    default:
      return kind
  }
}
