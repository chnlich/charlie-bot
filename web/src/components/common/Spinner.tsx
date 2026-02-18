import { clsx } from 'clsx'

export function Spinner({ size = 'sm', className }: { size?: 'sm' | 'md' | 'lg'; className?: string }) {
  return (
    <div
      className={clsx(
        'animate-spin rounded-full border-2 border-slate-600 border-t-blue-400',
        size === 'sm' && 'h-4 w-4',
        size === 'md' && 'h-6 w-6',
        size === 'lg' && 'h-8 w-8',
        className,
      )}
    />
  )
}
