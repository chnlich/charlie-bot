import client from './client'
import type { ThreadMetadata, WorkerEvent } from '../types'

export const threadsApi = {
  get: (sessionId: string, threadId: string) =>
    client.get<ThreadMetadata>(`/threads/${sessionId}/threads/${threadId}`).then((r) => r.data),

  getEvents: (sessionId: string, threadId: string) =>
    client.get<WorkerEvent[]>(`/threads/${sessionId}/threads/${threadId}/events`).then((r) => r.data),

  approvePlan: (sessionId: string, threadId: string, approvedSteps: string[], editedSteps?: string[]) =>
    client
      .post(`/threads/${sessionId}/threads/${threadId}/approve-plan`, {
        approved_steps: approvedSteps,
        edited_steps: editedSteps,
      })
      .then((r) => r.data),

  cancel: (sessionId: string, threadId: string) =>
    client.post(`/threads/${sessionId}/threads/${threadId}/cancel`).then((r) => r.data),
}
