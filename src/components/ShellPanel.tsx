// Run Command on Selection: the input dialog, and the output panel under the file list.
import { useEffect, useMemo, useRef, useState } from 'react'
import { Copy, Square, X } from 'lucide-react'
import { toast } from 'sonner'

import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { api, errorText, events } from '@/lib/api'
import { basename } from '@/lib/format'
import { useExplorer, type ShellInvocation, type ShellRunView, type ShellTarget } from '@/store/explorer'

/** Command lines shown in the preview before "N more". */
const PREVIEW_SHOWN = 5
const PLACEHOLDER_HELP = '{} file · {+} all files in one run · {dir} folder · {name} {stem} {ext} for each file'

const RUN_STATUS: Record<ShellRunView['status'], string> = {
  running: 'Running…',
  done: 'Finished',
  cancelled: 'Cancelled',
  stopped: 'Stopped at a failure',
}

function invocationStatus(inv: ShellInvocation): string {
  switch (inv.status) {
    case 'pending':
      return 'waiting'
    case 'running':
      return 'running…'
    case 'ok':
      return 'exit 0'
    case 'failed':
      return inv.code === null ? 'killed' : `exit ${inv.code}`
    case 'cancelled':
      return 'cancelled'
    case 'skipped':
      return 'not run'
  }
}

/** The input for a command, opened from the palette or Ctrl/⌘+Shift+R. */
export function ShellRunDialog() {
  const target = useExplorer((s) => s.shellTarget)
  const closeShell = useExplorer((s) => s.closeShell)
  if (!target) return null
  // Keyed by opening, so each time the dialog opens its form starts empty.
  return <ShellRunForm key={target.opened} target={target} onClose={closeShell} />
}

function ShellRunForm({ target, onClose }: { target: ShellTarget; onClose: () => void }) {
  const history = useExplorer((s) => s.shellHistory)
  const running = useExplorer((s) => s.shellRun?.status === 'running')
  const runShell = useExplorer((s) => s.runShell)
  const [template, setTemplate] = useState('')
  const [stopOnError, setStopOnError] = useState(true)
  const [plan, setPlan] = useState<string[]>([])
  const [planFailure, setPlanFailure] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    const t = setTimeout(() => inputRef.current?.focus(), 30)
    return () => clearTimeout(t)
  }, [])

  // The preview is the exact list of command lines the backend will run. The
  // backend plans again when the run starts, so this is only what the user sees.
  useEffect(() => {
    if (!template.trim()) return
    let live = true
    const t = setTimeout(() => {
      api
        .planShell(template, target.paths, target.dir)
        .then((lines) => {
          if (!live) return
          setPlan(lines)
          setPlanFailure(null)
        })
        .catch((e: unknown) => {
          if (!live) return
          setPlan([])
          setPlanFailure(errorText(e))
        })
    }, 150)
    return () => {
      live = false
      clearTimeout(t)
    }
  }, [template, target])

  // An empty command shows no preview, so a plan from earlier text is hidden here.
  const typed = template.trim() !== ''
  const preview = typed ? plan : []
  const planError = typed ? planFailure : null
  const canRun = typed && preview.length > 0 && !planError && !running && !busy
  const count = target.paths.length

  const submit = async () => {
    if (!canRun) return
    setBusy(true)
    setError(null)
    try {
      await runShell(template, stopOnError)
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>Run Command on Selection</DialogTitle>
          <DialogDescription>
            {count} {count === 1 ? 'item' : 'items'} in {basename(target.dir) || target.dir}. The command runs in that folder.
          </DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(e) => {
            e.preventDefault()
            void submit()
          }}
          className="grid gap-3"
        >
          <div className="flex gap-2">
            <Input
              ref={inputRef}
              value={template}
              spellCheck={false}
              placeholder="e.g. convert {} {stem}.webp"
              className="font-mono text-xs"
              onChange={(e) => setTemplate(e.target.value)}
            />
            {history.length > 0 && (
              <select
                aria-label="Recent commands"
                value=""
                onChange={(e) => e.target.value && setTemplate(e.target.value)}
                className="h-8 max-w-[11rem] rounded-md border border-input bg-background px-2 text-xs"
              >
                <option value="">Recent…</option>
                {history.map((h) => (
                  <option key={h} value={h}>
                    {h}
                  </option>
                ))}
              </select>
            )}
          </div>
          <p className="text-xs text-muted-foreground">{PLACEHOLDER_HELP}</p>

          {planError ? (
            <p role="alert" className="text-xs text-destructive">
              {planError}
            </p>
          ) : (
            preview.length > 0 && (
              <div className="rounded-md border bg-muted/40 p-2 font-mono text-xs">
                <div className="mb-1 font-sans text-muted-foreground">
                  Runs {preview.length === 1 ? 'once' : `${preview.length} times`}:
                </div>
                {preview.slice(0, PREVIEW_SHOWN).map((line, i) => (
                  <div key={i} className="truncate" title={line}>
                    {line}
                  </div>
                ))}
                {preview.length > PREVIEW_SHOWN && (
                  <div className="text-muted-foreground">{preview.length - PREVIEW_SHOWN} more</div>
                )}
              </div>
            )
          )}

          <label className="flex items-center gap-2 text-xs">
            <input type="checkbox" checked={stopOnError} onChange={(e) => setStopOnError(e.target.checked)} />
            Stop at the first failure
          </label>
          {running && <p className="text-xs text-muted-foreground">A command is still running. Cancel it first.</p>}
          {error && (
            <p role="alert" className="text-xs text-destructive">
              {error}
            </p>
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!canRun}>
              Run
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

/** Live output of the last command, shown under the file list. Also listens for the backend's events. */
export function ShellPanel() {
  const run = useExplorer((s) => s.shellRun)
  const open = useExplorer((s) => s.shellPanelOpen)
  const cancelShell = useExplorer((s) => s.cancelShell)
  const closeShellPanel = useExplorer((s) => s.closeShellPanel)
  const scrollRef = useRef<HTMLDivElement>(null)
  /** Follow new output only while the view is at the bottom. */
  const follow = useRef(true)

  useEffect(() => {
    const st = useExplorer.getState
    const subs = [
      events.onShellLines((e) => st().onShellLines(e)),
      events.onShellExit((e) => st().onShellExit(e)),
      events.onShellDone((e) => st().onShellDone(e)),
    ]
    return () => {
      for (const p of subs) void p.then((un) => un())
    }
  }, [])

  const byInvocation = useMemo(() => {
    const m = new Map<number, ShellRunView['lines']>()
    for (const l of run?.lines ?? []) {
      const list = m.get(l.inv)
      if (list) list.push(l)
      else m.set(l.inv, [l])
    }
    return m
  }, [run?.lines])

  useEffect(() => {
    const el = scrollRef.current
    if (el && follow.current) el.scrollTop = el.scrollHeight
  }, [run?.lines.length, run?.invocations.length, open])

  if (!open || !run) return null

  const copyOutput = async () => {
    const text = run.invocations
      .map((inv, i) => [`$ ${inv.command}`, ...(byInvocation.get(i) ?? []).map((l) => l.text)].join('\n'))
      .join('\n\n')
    try {
      await navigator.clipboard.writeText(text)
      toast('Copied output')
    } catch {
      toast.error('Clipboard unavailable')
    }
  }

  return (
    <section aria-label="Command output" className="flex h-64 shrink-0 flex-col border-t bg-background">
      <header className="flex items-center gap-2 border-b px-3 py-1.5">
        <span className="text-sm font-medium">Command output</span>
        <span className="text-xs text-muted-foreground">{RUN_STATUS[run.status]}</span>
        <div className="ml-auto flex items-center gap-1.5">
          {run.status === 'running' && (
            <Button size="sm" variant="outline" onClick={() => cancelShell()}>
              <Square />
              Cancel
            </Button>
          )}
          <Button size="sm" variant="outline" disabled={run.lines.length === 0} onClick={() => void copyOutput()}>
            <Copy />
            Copy Output
          </Button>
          <Button size="icon" variant="ghost" aria-label="Close output" onClick={() => closeShellPanel()}>
            <X />
          </Button>
        </div>
      </header>
      <div
        ref={scrollRef}
        onScroll={(e) => {
          const el = e.currentTarget
          follow.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24
        }}
        className="min-h-0 flex-1 overflow-y-auto px-3 py-2 font-mono text-xs"
      >
        {run.firstSeq > 0 && (
          <div className="mb-2 font-sans text-muted-foreground">
            {run.firstSeq} earlier {run.firstSeq === 1 ? 'line was' : 'lines were'} dropped to keep the output small
          </div>
        )}
        {run.invocations.map((inv, i) => (
          <div key={i} className="mb-2">
            <div className="flex items-baseline gap-2 border-b pb-0.5">
              <span className="truncate font-semibold">$ {inv.command}</span>
              <span className="ml-auto shrink-0 font-sans text-muted-foreground">{invocationStatus(inv)}</span>
            </div>
            {(byInvocation.get(i) ?? []).map((l) => (
              <div
                key={l.seq}
                className={`whitespace-pre-wrap break-all ${l.err ? 'text-orange-600 dark:text-orange-400' : ''}`}
              >
                {l.text || ' '}
              </div>
            ))}
          </div>
        ))}
      </div>
    </section>
  )
}
