// 行内二次确认按钮：首次点击展开「确认/取消」，确认后执行 onConfirm。
// 供拆链、删除规则、删除设备等不可逆操作复用；busy 时禁用并显示 busyLabel。
import { useState } from 'react'

interface Props {
  label: string
  confirmLabel: string
  busyLabel: string
  busy: boolean
  disabled?: boolean
  onConfirm: () => void
}

export default function ConfirmButton({ label, confirmLabel, busyLabel, busy, disabled, onConfirm }: Props) {
  const [open, setOpen] = useState(false)
  if (busy) {
    return (
      <button type="button" disabled className="rounded bg-neutral-800 px-3 py-1.5 text-sm opacity-50">
        {busyLabel}
      </button>
    )
  }
  if (open) {
    return (
      <span className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => {
            setOpen(false)
            onConfirm()
          }}
          className="rounded bg-red-700 px-3 py-1.5 text-sm text-white hover:bg-red-600"
        >
          {confirmLabel}
        </button>
        <button type="button" onClick={() => setOpen(false)} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
          取消
        </button>
      </span>
    )
  }
  return (
    <button type="button" disabled={disabled} onClick={() => setOpen(true)} className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-red-300 hover:bg-neutral-700 disabled:opacity-50">
      {label}
    </button>
  )
}
