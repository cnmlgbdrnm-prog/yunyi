// 复制文本到剪贴板：优先 Clipboard API，失败落 execCommand 兜底，返回是否真的复制上了。
//
// **为什么需要这一层**：`navigator.clipboard.writeText` 返回 Promise，文档失焦（用户切到
// 别的窗口）或权限被拒时它会 **reject**。裸写 `void navigator.clipboard.writeText(x)`
// （Shares.tsx 原先就是）产生的未处理拒绝既没有兜底、也没有提示——用户点了「复制链接」
// 什么都没发生，界面上一点动静都没有。PeerConfigDialog.tsx:58-79 早就把这件事做对了，
// 这里只是把那套做法收敛成一份，免得下一个调用点再漏一次。
//
// **兜底为什么自建 textarea**：`execCommand('copy')` 复制的是当前**选区**；页面上那段
// URL 在 <td> 里，没有 `.select()` 可用。选区为空时 execCommand 会「成功」地复制走空
// 字符串——那等于用一句谎话换掉一次静默失败。自建 textarea 承担选区来源，两条路都失败
// 才返回 false，由调用方如实报错。

/** execCommand 兜底：临时 textarea 承担选区，用完即摘（不在页面上留 dom 痕迹）。 */
function legacyCopy(text: string): boolean {
  const ta = document.createElement('textarea')
  ta.value = text
  // 固定在视口外，复制期间页面不跳动；readOnly 防移动端输入法弹起
  ta.setAttribute('readonly', '')
  ta.style.position = 'fixed'
  ta.style.top = '-1000px'
  document.body.appendChild(ta)
  try {
    ta.select()
    // execCommand 在部分环境（jsdom、无 WebView 的宿主）根本不存在，调用即 TypeError
    return document.execCommand('copy')
  } catch {
    return false
  } finally {
    document.body.removeChild(ta)
  }
}

/**
 * 复制 `text`，返回是否成功。
 *
 * 调用方**必须**处理 false：静默的复制失败正是这条路径要消灭的东西。
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    // 非安全上下文里 navigator.clipboard 整个是 undefined，故用可选链探测而非直接调用
    if (navigator.clipboard?.writeText != null) {
      await navigator.clipboard.writeText(text)
      return true
    }
  } catch {
    // 剪贴板 API 不可用或被拒：落到 execCommand 兜底
  }
  return legacyCopy(text)
}
