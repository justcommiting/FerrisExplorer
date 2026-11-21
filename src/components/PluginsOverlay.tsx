import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'

import { reloadPlugins, startPluginRun } from '@/commands'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { type ShortcutCheck, pluginShortcuts } from '@/hooks/useShortcuts'
import { api, errorText, type PluginAction, type PluginView, type PluginWhen } from '@/lib/api'
import { cn } from '@/lib/utils'
import { useExplorer } from '@/store/explorer'

/** A plain-language summary of a `when` rule, for the list. */
function describeWhen(w: PluginWhen): string {
  const kind = w.kind === 'any' ? 'files or folders' : w.kind
  const exts = w.extensions.length > 0 ? ` with .${w.extensions.join(', .')}` : ''
  const count =
    w.max === null ? `${w.min}+ selected` : w.min === w.max ? `${w.min} selected` : `${w.min}–${w.max} selected`
  return `${kind}${exts}, ${count}`
}

/** Whether an action's shortcut is bound, and if not, why. `check` is missing while the plugin is off. */
function ShortcutNote({ text, check }: { text: string; check: ShortcutCheck | undefined }) {
  if (!check) {
    return <span className="text-[11.5px] text-muted-foreground">Shortcut {text} is bound once the plugin is enabled</span>
  }
  switch (check.state) {
    case 'bound':
      return <span className="text-[11.5px] text-muted-foreground">Shortcut {text} is bound</span>
    case 'builtin':
      return <span className="text-[11.5px] text-destructive">Shortcut {text} is a built-in key, so it is not bound</span>
    case 'taken':
      return (
        <span className="text-[11.5px] text-destructive">
          Shortcut {text} is already used by {check.by}, so it is not bound
        </span>
      )
    case 'invalid':
      return (
        <span className="text-[11.5px] text-destructive">
          Shortcut {text} cannot be bound. Use Ctrl or ⌘ with a letter, digit, F1–F12, Enter, Space, Delete or Backspace.
        </span>
      )
  }
}

function ActionRow({ action, check }: { action: PluginAction; check: ShortcutCheck | undefined }) {
  return (
    <li className="flex flex-col gap-0.5 border-t border-border py-2 first:border-t-0">
      <div className="flex flex-wrap items-center gap-2 text-[13px]">
        <span className="font-medium">{action.label}</span>
        {action.confirm && <Badge variant="outline">asks first</Badge>}
      </div>
      <code className="selectable font-mono text-[11.5px] break-all text-foreground/80">{action.command}</code>
      <span className="text-[11.5px] text-muted-foreground">Runs on {describeWhen(action.when)}</span>
      {action.shortcut && <ShortcutNote text={action.shortcut} check={check} />}
    </li>
  )
}

function PluginCard({
  plugin,
  busy,
  onToggle,
  checks,
}: {
  plugin: PluginView
  busy: boolean
  onToggle: () => void
  checks: Map<string, ShortcutCheck>
}) {
  return (
    <section className="flex flex-col gap-2 rounded-md border bg-card p-3">
      <div className="flex flex-wrap items-center gap-2">
        <h3 className="text-sm font-semibold">{plugin.name}</h3>
        <span className="text-xs text-muted-foreground">v{plugin.version}</span>
        <Badge variant={plugin.enabled ? 'default' : 'secondary'}>{plugin.enabled ? 'Enabled' : 'Disabled'}</Badge>
        <Button size="sm" variant={plugin.enabled ? 'outline' : 'default'} className="ml-auto" disabled={busy} onClick={onToggle}>
          {plugin.enabled ? 'Disable' : 'Enable'}
        </Button>
      </div>
      {plugin.description && <p className="text-xs text-muted-foreground">{plugin.description}</p>}
      <p className="text-[11px] text-muted-foreground">
        File <span className="font-mono">{plugin.file}</span>
        {!plugin.enabled && ' · new or changed since it was last enabled, so it stays off until you enable it'}
      </p>
      {plugin.actions.length > 0 && (
        <div>
          <h4 className="text-[11px] tracking-wide text-muted-foreground uppercase">Actions</h4>
          <ul>
            {plugin.actions.map((a) => (
              <ActionRow key={a.id} action={a} check={checks.get(`${plugin.file}#${a.id}`)} />
            ))}
          </ul>
        </div>
      )}
      {plugin.previewers.length > 0 && (
        <div>
          <h4 className="text-[11px] tracking-wide text-muted-foreground uppercase">Previewers</h4>
          <ul>
            {plugin.previewers.map((p, i) => (
              <li key={i} className="flex flex-col gap-0.5 border-t border-border py-2 first:border-t-0">
                <span className="text-[12px]">.{p.extensions.join(', .')}</span>
                <code className="selectable font-mono text-[11.5px] break-all text-foreground/80">{p.command}</code>
                <span className="text-[11.5px] text-muted-foreground">
                  Stops after {p.timeout_ms} ms, shows up to {Math.round(p.max_output / 1024)} KiB
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </section>
  )
}

/** The Plugins window: every plugin file, its commands, enable/disable, and files that failed to load. */
export function PluginsOverlay() {
  const open = useExplorer((s) => s.pluginsOpen)
  const report = useExplorer((s) => s.pluginReport)
  const set = useExplorer((s) => s.set)
  const [busy, setBusy] = useState<string | null>(null)
  // Which shortcuts are bound, and which collide with a built-in key or another plugin.
  const checks = useMemo(() => pluginShortcuts(report).checks, [report])

  // Load at startup. The palette stays mounted for the whole session, so this runs once.
  useEffect(() => {
    void reloadPlugins(true)
  }, [])

  async function toggle(p: PluginView) {
    setBusy(p.file)
    try {
      set({ pluginReport: await api.setPluginEnabled(p.file, p.hash, !p.enabled) })
    } catch (e) {
      toast.error(errorText(e))
      // The file may have changed since it was listed, so show what is on disk now.
      void reloadPlugins(true)
    } finally {
      setBusy(null)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => set({ pluginsOpen: o })}>
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Plugins</DialogTitle>
          <DialogDescription>
            Plugins are declarative files in <span className="font-mono">{report?.dir ?? '~/.ferris_explorer/plugins'}</span>. A
            plugin does nothing until you enable it, and editing its file turns it off again.
          </DialogDescription>
        </DialogHeader>

        {report && report.plugins.length === 0 && report.errors.length === 0 && (
          <p className="text-sm text-muted-foreground">No plugin files yet. Copy one in and choose Reload Plugins.</p>
        )}
        <div className="flex flex-col gap-3">
          {report?.plugins.map((p) => (
            <PluginCard key={p.file} plugin={p} busy={busy === p.file} onToggle={() => void toggle(p)} checks={checks} />
          ))}
        </div>
        {report && report.errors.length > 0 && (
          <div className={cn('flex flex-col gap-2 rounded-md border border-destructive/60 bg-destructive/10 p-3')}>
            <h3 className="text-sm font-semibold text-destructive">Could not load</h3>
            <ul className="flex flex-col gap-1.5">
              {report.errors.map((e) => (
                <li key={e.file} className="text-xs">
                  <span className="font-mono">{e.file}</span>: {e.error}
                </li>
              ))}
            </ul>
          </div>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={() => void reloadPlugins()}>
            Reload Plugins
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/** Shows the command lines of a confirmed plugin action, and runs them only on Run. */
export function PluginRequestDialog() {
  const req = useExplorer((s) => s.pluginRequest)
  const set = useExplorer((s) => s.set)
  const close = () => set({ pluginRequest: null })

  return (
    <Dialog open={req !== null} onOpenChange={(o) => !o && close()}>
      <DialogContent className="sm:max-w-lg">
        {req && (
          <>
            <DialogHeader>
              <DialogTitle>{req.title}</DialogTitle>
              <DialogDescription>
                These commands run in <span className="font-mono">{req.dir}</span> on {req.paths.length} selected item(s):
              </DialogDescription>
            </DialogHeader>
            <pre className="selectable max-h-64 overflow-auto rounded-md border bg-field p-3 font-mono text-xs whitespace-pre-wrap">
              {req.plan.lines.join('\n')}
            </pre>
            <DialogFooter>
              <Button variant="outline" onClick={close}>
                Cancel
              </Button>
              <Button
                onClick={() => {
                  void startPluginRun(req)
                  close()
                }}
              >
                Run
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  )
}
