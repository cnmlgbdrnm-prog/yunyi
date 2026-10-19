export default function NavItem({
  label,
  active,
  badge = 0,
  onClick,
}: {
  label: string
  active: boolean
  badge?: number
  onClick: () => void
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`rounded px-3 py-2 text-left transition-colors ${
        active
          ? 'bg-neutral-800 text-white'
          : 'text-neutral-400 hover:bg-neutral-900 hover:text-neutral-200'
      }`}
    >
      <span className="flex items-center justify-between gap-2">
        <span>{label}</span>
        {badge > 0 && (
          <span
            aria-label={`${badge} 条未确认告警`}
            className="rounded-full bg-red-600 px-1.5 text-xs text-white"
          >
            {badge > 99 ? '99+' : badge}
          </span>
        )}
      </span>
    </button>
  )
}
