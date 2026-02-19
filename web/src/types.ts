// TypeScript interfaces mirroring CharlieBot Pydantic models

export type Priority = 'P0' | 'P1' | 'P2'
export type TaskStatus = 'pending' | 'running' | 'completed' | 'failed' | 'pending_quota' | 'cancelled'
export type ThreadStatus =
  | 'idle'
  | 'planning'
  | 'running'
  | 'awaiting_approval'
  | 'completed'
  | 'failed'
  | 'conflict'
  | 'cancelled'
export type SessionStatus = 'active' | 'archived'
export type MessageRole = 'user' | 'assistant' | 'system'

export interface Task {
  id: string
  priority: Priority
  description: string
  created_at: string
  status: TaskStatus
  thread_id: string | null
  plan_steps: string[] | null
  is_plan_mode: boolean
  context: Record<string, unknown>
}

export interface TaskQueue {
  session_id: string
  tasks: Task[]
  updated_at: string
}

export interface ThreadMetadata {
  id: string
  session_id: string
  task_id: string
  description: string
  branch_name: string
  status: ThreadStatus
  created_at: string
  started_at: string | null
  completed_at: string | null
  pid: number | null
  exit_code: number | null
  cli_command: string | null
  worktree_path: string | null
  is_conflict_resolver: boolean
}

export interface SessionMetadata {
  id: string
  name: string
  repo_path: string | null
  status: SessionStatus
  created_at: string
  updated_at: string
}

export interface ChatMessage {
  id: string
  role: MessageRole
  content: string
  timestamp: string
  is_voice: boolean
  thread_id: string | null
}

export interface ConversationHistory {
  session_id: string
  messages: ChatMessage[]
  summary: string | null
}

export interface WorkerEvent {
  type: string
  content?: string
  path?: string
  lines_added?: number
  message?: string
  status?: string
  timestamp?: string
  exit_code?: number
}

export interface CreateSessionRequest {
  name?: string
  repo_path?: string
}

export interface VoiceTranscriptionResponse {
  transcription: string
  disclaimer: string
}

// SSE event types from chat endpoint
export interface SSEChunkEvent {
  type: 'chunk'
  content: string
}

export interface SSETaskDelegatedEvent {
  type: 'task_delegated'
  task_id: string
  priority: Priority
  description: string
  plan_mode: boolean
}

export interface SSEDoneEvent {
  type: 'done'
}

export type SSEEvent = SSEChunkEvent | SSETaskDelegatedEvent | SSEDoneEvent | { type: string }
