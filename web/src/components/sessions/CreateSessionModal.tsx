import { useEffect, useState } from 'react'
import { Modal } from '../common/Modal'
import { sessionsApi, type ProjectInfo } from '../../api/sessions'
import { useSessionsStore } from '../../store/sessions'

interface Props {
  open: boolean
  onClose: () => void
}

export function CreateSessionModal({ open, onClose }: Props) {
  const [name, setName] = useState('')
  const [repoPath, setRepoPath] = useState('')
  const [baseBranch, setBaseBranch] = useState('main')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')
  const [projects, setProjects] = useState<ProjectInfo[]>([])
  const addSession = useSessionsStore((s) => s.addSession)
  const setActiveSession = useSessionsStore((s) => s.setActiveSession)

  useEffect(() => {
    if (open) {
      sessionsApi.listProjects().then(setProjects).catch(() => setProjects([]))
    }
  }, [open])

  const handleCreate = async () => {
    if (!name.trim()) { setError('Name is required'); return }
    setLoading(true)
    setError('')
    try {
      const session = await sessionsApi.create({
        name: name.trim(),
        repo_path: repoPath || undefined,
        base_branch: baseBranch.trim() || 'main',
      })
      addSession(session)
      setActiveSession(session.id)
      onClose()
      setName('')
      setRepoPath('')
      setBaseBranch('main')
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to create session')
    } finally {
      setLoading(false)
    }
  }

  return (
    <Modal open={open} onClose={onClose} title="New Session">
      <div className="space-y-3">
        <div>
          <label className="block text-xs text-slate-400 mb-1">Session name</label>
          <input
            className="w-full bg-slate-800 border border-border rounded px-3 py-2 text-white text-sm focus:outline-none focus:border-blue-500"
            placeholder="My Project"
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && handleCreate()}
          />
        </div>
        <div>
          <label className="block text-xs text-slate-400 mb-1">Project repository</label>
          {projects.length > 0 ? (
            <select
              className="w-full bg-slate-800 border border-border rounded px-3 py-2 text-white text-sm focus:outline-none focus:border-blue-500"
              value={repoPath}
              onChange={(e) => setRepoPath(e.target.value)}
            >
              <option value="">None (no git repo)</option>
              {projects.map((p) => (
                <option key={p.path} value={p.path}>
                  {p.name} — {p.path}
                </option>
              ))}
            </select>
          ) : (
            <input
              className="w-full bg-slate-800 border border-border rounded px-3 py-2 text-white text-sm focus:outline-none focus:border-blue-500"
              placeholder="/home/user/my-project"
              value={repoPath}
              onChange={(e) => setRepoPath(e.target.value)}
            />
          )}
        </div>
        <div>
          <label className="block text-xs text-slate-400 mb-1">Base branch</label>
          <input
            className="w-full bg-slate-800 border border-border rounded px-3 py-2 text-white text-sm focus:outline-none focus:border-blue-500"
            placeholder="main"
            value={baseBranch}
            onChange={(e) => setBaseBranch(e.target.value)}
          />
        </div>
        {error && <p className="text-red-400 text-xs">{error}</p>}
        <button
          onClick={handleCreate}
          disabled={loading}
          className="w-full bg-blue-600 hover:bg-blue-700 disabled:opacity-50 text-white rounded py-2 text-sm font-medium mt-2"
        >
          {loading ? 'Creating...' : 'Create Session'}
        </button>
      </div>
    </Modal>
  )
}
