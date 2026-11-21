// Git dialogs for the active repository: commit the index, and confirm a discard.
import { useState } from 'react'

import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { Textarea } from '@/components/ui/textarea'
import { basename, relativeTo } from '@/lib/format'
import { useExplorer } from '@/store/explorer'

/**
 * Commits what is staged, with `git commit -m`. The message is one argument, never
 * a shell string. The repository's commit hooks run, as they would in a terminal.
 * Nothing is pushed and nothing is amended.
 */
export function CommitDialog() {
  const open = useExplorer((s) => s.commitOpen)
  const set = useExplorer((s) => s.set)
  return (
    <Dialog open={open} onOpenChange={(o) => set({ commitOpen: o })}>
      <DialogContent className="sm:max-w-lg">{open && <CommitForm />}</DialogContent>
    </Dialog>
  )
}

/** Mounted only while the dialog is open, so each opening starts with an empty message. */
function CommitForm() {
  // Select the tab object itself: a fresh array per call would never settle in the store.
  const git = useExplorer((s) => s.activeTab().git)
  const gitCommit = useExplorer((s) => s.gitCommit)
  const set = useExplorer((s) => s.set)
  const [message, setMessage] = useState('')
  const [busy, setBusy] = useState(false)

  const staged = git?.staged ?? []
  const canCommit = staged.length > 0 && message.trim().length > 0 && !busy

  const submit = async () => {
    if (!canCommit) return
    setBusy(true)
    const ok = await gitCommit(message)
    setBusy(false)
    if (ok) set({ commitOpen: false })
  }

  return (
    <>
      <DialogHeader>
        <DialogTitle>Commit staged changes</DialogTitle>
        <DialogDescription>
          On <span className="font-mono">{git?.branch}</span>. Only the staged files below are committed. Your repository&apos;s
          commit hooks run. Nothing is pushed.
        </DialogDescription>
      </DialogHeader>
      <div className="max-h-40 overflow-auto rounded-md border bg-field p-2 font-mono text-xs">
        {staged.length === 0 ? (
          <div className="text-dim">Nothing is staged. Select files and choose Stage first.</div>
        ) : (
          staged.map((p) => (
            <div key={p} className="truncate" title={p}>
              {git ? relativeTo(p, git.root) : p}
            </div>
          ))
        )}
      </div>
      <Textarea
        autoFocus
        rows={5}
        value={message}
        placeholder="Commit message"
        aria-label="Commit message"
        onChange={(e) => setMessage(e.target.value)}
        onKeyDown={(e) => {
          if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') void submit()
        }}
      />
      <DialogFooter>
        <Button variant="outline" disabled={busy} onClick={() => set({ commitOpen: false })}>
          Cancel
        </Button>
        <Button disabled={!canCommit} onClick={() => void submit()}>
          {busy ? 'Committing…' : 'Commit'}
        </Button>
      </DialogFooter>
    </>
  )
}

/** Confirms Discard Changes. The store copies each file to the Trash before reverting it. */
export function DiscardDialog() {
  const paths = useExplorer((s) => s.confirmDiscard)
  const set = useExplorer((s) => s.set)
  const gitDiscard = useExplorer((s) => s.gitDiscard)
  const [busy, setBusy] = useState(false)

  const confirm = async () => {
    if (!paths) return
    setBusy(true)
    await gitDiscard(paths)
    setBusy(false)
    set({ confirmDiscard: null })
  }

  const count = paths?.length ?? 0
  return (
    <AlertDialog open={!!paths} onOpenChange={(o) => !o && !busy && set({ confirmDiscard: null })}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>
            Discard changes to {count === 1 ? 'this file' : `${count} files`}?
          </AlertDialogTitle>
          <AlertDialogDescription>
            Each file goes back to its staged (or committed) version. A copy of its current content goes to the Trash first, so
            it can be recovered. Untracked files are removed after that copy.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <ul className="max-h-40 overflow-auto rounded-md border bg-field p-2 font-mono text-xs">
          {paths?.map((p) => (
            <li key={p} className="truncate" title={p}>
              {basename(p)}
            </li>
          ))}
        </ul>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={busy}>Cancel</AlertDialogCancel>
          <Button variant="destructive" disabled={busy} onClick={() => void confirm()}>
            Discard Changes
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}
