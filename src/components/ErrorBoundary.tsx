import { Component, type ErrorInfo, type ReactNode } from 'react'
import { Button } from '@/components/ui/button'
import { api, errorText } from '@/lib/api'

interface Props {
  children: ReactNode
}

interface State {
  error: unknown
  failed: boolean
}

/** Fire-and-forget: a logging failure must never throw inside the boundary. */
function report(message: string, stack?: string) {
  try {
    void api.logFrontendError(message, stack).catch(() => {})
  } catch {
    // The IPC bridge is unavailable; there is nowhere left to report to.
  }
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: undefined, failed: false }

  static getDerivedStateFromError(error: unknown): State {
    return { error, failed: true }
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    const stack = `${error instanceof Error ? (error.stack ?? '') : ''}${info.componentStack ?? ''}`
    report(errorText(error), stack)
  }

  render() {
    if (!this.state.failed) return this.props.children

    return (
      <div className="flex h-full min-h-screen w-full flex-col items-center justify-center gap-4 p-4 text-center">
        <h1 className="text-lg font-semibold">Something went wrong</h1>
        <pre className="bg-muted text-muted-foreground max-h-40 w-full max-w-lg overflow-auto rounded-md p-3 text-left font-mono text-xs whitespace-pre-wrap break-words">
          {errorText(this.state.error)}
        </pre>
        <div className="flex gap-2">
          <Button variant="outline" onClick={() => window.location.reload()}>
            Reload
          </Button>
          <Button
            variant="outline"
            onClick={() => {
              api.openLogDir().catch(() => {})
            }}
          >
            Open log folder
          </Button>
        </div>
      </div>
    )
  }
}
