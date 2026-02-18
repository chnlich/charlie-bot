import client from './client'
import type { CreateSessionRequest, SessionMetadata, TaskQueue, ThreadMetadata } from '../types'

export interface ProjectInfo {
  name: string
  path: string
}

export const sessionsApi = {
  list: () => client.get<SessionMetadata[]>('/sessions/').then((r) => r.data),

  create: (req: CreateSessionRequest) =>
    client.post<SessionMetadata>('/sessions/', req).then((r) => r.data),

  listProjects: () =>
    client.get<ProjectInfo[]>('/sessions/projects').then((r) => r.data),

  get: (id: string) => client.get<SessionMetadata>(`/sessions/${id}`).then((r) => r.data),

  archive: (id: string) => client.delete(`/sessions/${id}`).then((r) => r.data),

  listThreads: (sessionId: string) =>
    client.get<ThreadMetadata[]>(`/sessions/${sessionId}/threads`).then((r) => r.data),

  getQueue: (sessionId: string) =>
    client.get<TaskQueue>(`/sessions/${sessionId}/queue`).then((r) => r.data),

  reorderTask: (sessionId: string, taskId: string, priority: string) =>
    client.post(`/sessions/${sessionId}/queue/reorder`, { task_id: taskId, priority }).then((r) => r.data),

  cancelTask: (sessionId: string, taskId: string) =>
    client.delete(`/sessions/${sessionId}/queue/${taskId}`).then((r) => r.data),
}
