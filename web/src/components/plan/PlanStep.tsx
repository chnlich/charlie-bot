import { useState } from 'react'
import { Check, Pencil, X } from 'lucide-react'
import { clsx } from 'clsx'

interface Props {
  step: string
  index: number
  checked: boolean
  onToggle: (index: number) => void
  onEdit: (index: number, newText: string) => void
}

export function PlanStep({ step, index, checked, onToggle, onEdit }: Props) {
  const [editing, setEditing] = useState(false)
  const [editText, setEditText] = useState(step)

  const commit = () => {
    onEdit(index, editText.trim() || step)
    setEditing(false)
  }

  return (
    <div className={clsx('flex items-start gap-2 py-1.5', !checked && 'opacity-50')}>
      <button
        onClick={() => onToggle(index)}
        className={clsx(
          'mt-0.5 w-4 h-4 rounded border shrink-0 flex items-center justify-center transition-colors',
          checked ? 'bg-blue-600 border-blue-600' : 'border-slate-600',
        )}
      >
        {checked && <Check size={10} className="text-white" />}
      </button>

      {editing ? (
        <div className="flex-1 flex gap-1">
          <input
            autoFocus
            className="flex-1 bg-slate-800 border border-blue-500 rounded px-2 py-0.5 text-xs text-white focus:outline-none"
            value={editText}
            onChange={(e) => setEditText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') commit()
              if (e.key === 'Escape') setEditing(false)
            }}
          />
          <button onClick={commit} className="text-green-400 hover:text-green-300"><Check size={12} /></button>
          <button onClick={() => setEditing(false)} className="text-red-400 hover:text-red-300"><X size={12} /></button>
        </div>
      ) : (
        <div className="flex-1 flex items-start gap-1 group">
          <span className="text-xs text-slate-300 flex-1">{step}</span>
          <button
            onClick={() => { setEditText(step); setEditing(true) }}
            className="opacity-0 group-hover:opacity-100 text-slate-500 hover:text-slate-300 transition-opacity shrink-0"
          >
            <Pencil size={10} />
          </button>
        </div>
      )}
    </div>
  )
}
