// 设备配置弹窗：getPeerConfig 拉取 conf 文本，提供二维码 / 复制 / 下载 .conf 三入口。
// 二维码经 qrcode 库转 dataURL（组件内生成，卸载时置空）；复制优先走剪贴板 API，
// 失败时选中文本兜底；下载经 Blob + objectURL 触发浏览器保存。
import { useEffect, useRef, useState } from 'react'
import QRCode from 'qrcode'
import { getPeerConfig } from '../api'
import type { DevicePeer } from '../api/types'

interface Props {
  peer: DevicePeer
  onClose: () => void
}

export default function PeerConfigDialog({ peer, onClose }: Props) {
  const [conf, setConf] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [qrUrl, setQrUrl] = useState<string | null>(null)
  const [copied, setCopied] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const load = async () => {
    setLoading(true)
    setError(null)
    try {
      setConf(await getPeerConfig(peer.id))
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    void load()
    // 组件卸载即清理：二维码对象与复制状态不跨弹窗残留
    return () => setQrUrl(null)
    // 只挂载时拉取一次，重试走 load 显式调用
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    if (conf == null) return
    let alive = true
    void QRCode.toDataURL(conf)
      .then((url) => {
        if (alive) setQrUrl(url)
      })
      .catch(() => {
        // 二维码生成失败不影响文本/复制/下载入口
      })
    return () => {
      alive = false
      setQrUrl(null)
    }
  }, [conf])

  const handleCopy = async () => {
    if (conf == null) return
    try {
      if (navigator.clipboard?.writeText != null) {
        await navigator.clipboard.writeText(conf)
        setCopied(true)
        return
      }
    } catch {
      // 剪贴板 API 不可用或拒绝时走选中文本兜底
    }
    const ta = textareaRef.current
    if (ta != null) {
      ta.select()
      try {
        document.execCommand('copy')
      } catch {
        // 兜底失败不再报错：用户仍可直接选中复制
      }
      setCopied(true)
    }
  }

  const handleDownload = () => {
    if (conf == null) return
    const blob = new Blob([conf], { type: 'text/plain' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `${peer.name}.conf`
    a.click()
    URL.revokeObjectURL(url)
  }

  return (
    <div className="fixed inset-0 z-20 flex items-center justify-center bg-black/60" role="dialog" aria-label={`${peer.name} 配置`}>
      <div className="w-full max-w-lg space-y-3 rounded border border-neutral-800 bg-neutral-900 p-4">
        <h3 className="text-lg font-semibold">{peer.name} 配置</h3>
        {error && (
          <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
            {error}
          </p>
        )}
        {conf == null && !error && <p className="text-sm text-neutral-400">配置生成中…</p>}
        {conf != null && (
          <>
            <div className="flex justify-center">
              {qrUrl != null ? (
                <img src={qrUrl} alt={`${peer.name} 配置二维码`} className="h-40 w-40 bg-white p-2" />
              ) : (
                <p className="text-sm text-neutral-500">二维码生成中…</p>
              )}
            </div>
            <textarea
              ref={textareaRef}
              readOnly
              rows={8}
              value={conf}
              className="w-full rounded border border-neutral-700 bg-neutral-950 px-2 py-1 font-mono text-xs"
            />
          </>
        )}
        <div className="flex gap-2">
          {conf != null && (
            <>
              <button type="button" onClick={() => void handleCopy()} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
                {copied ? '已复制' : '复制配置'}
              </button>
              <button type="button" onClick={handleDownload} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
                下载 .conf
              </button>
            </>
          )}
          {error != null && (
            <button type="button" onClick={() => void load()} disabled={loading} className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50">
              {loading ? '重试中…' : '重试'}
            </button>
          )}
          <button type="button" onClick={onClose} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
            关闭
          </button>
        </div>
      </div>
    </div>
  )
}
