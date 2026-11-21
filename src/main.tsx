import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.tsx'
import { ErrorBoundary } from '@/components/ErrorBoundary'
import { api } from '@/lib/api'

// Forwards uncaught errors to the app log. Each step is guarded so that a
// failure while reporting cannot raise another error event and loop.
function forward(message: string, stack?: string) {
  try {
    void api.logFrontendError(message, stack).catch(() => {})
  } catch {
    // IPC unavailable; drop the report.
  }
}

window.addEventListener('error', (event) => {
  if (!event.message && !event.error) return
  forward(event.message || String(event.error), event.error?.stack)
})

window.addEventListener('unhandledrejection', (event) => {
  const reason: unknown = event.reason
  if (reason instanceof Error) forward(reason.message, reason.stack)
  else forward(String(reason))
})

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>,
)
