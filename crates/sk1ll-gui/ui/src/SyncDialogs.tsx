import { useCallback, useState } from 'react';
import { Tabs } from '@base-ui/react/tabs';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { api, errorMessage } from './api';
import { useMutation, useResource } from './hooks';
import { DiffView, ErrorNotice, Loading, Pending } from './shared';
import { toast } from './toast';

export function InstructionsDialog({ harness, onClose }: { harness: string; onClose: () => void }) {
  const load = useCallback(() => api.renderInstructions(harness), [harness]);
  const preview = useResource(load, 0);
  return <Dialog open onOpenChange={open => { if (!open) onClose(); }}>
    <DialogContent className="sm:max-w-3xl">
      <DialogHeader><DialogTitle>Preview instructions for {harness}</DialogTitle>
        <DialogDescription>See the proposed files and how they compare with what is on disk.</DialogDescription></DialogHeader>
      <ErrorNotice error={preview.error} />
      {preview.loading && <Loading rows={2} />}
      {preview.data?.length === 0 && <p>No instructions selected for this Harness.</p>}
      {!!preview.data?.length && <Tabs.Root defaultValue={preview.data[0].path}>
        <Tabs.List aria-label="Instruction files" className="flex gap-1 overflow-x-auto border-b">
          {preview.data.map(file => <Tabs.Tab key={file.path} value={file.path} title={file.path}
            className="shrink-0 border-b-2 border-transparent px-3 py-2 font-medium text-muted-foreground data-active:border-primary data-active:text-foreground">
            {file.path.split('/').at(-1)}
          </Tabs.Tab>)}
        </Tabs.List>
        {preview.data.map(file => <Tabs.Panel key={file.path} value={file.path} className="space-y-3 pt-3">
          <p className="break-all font-mono text-xs text-muted-foreground">{file.path}</p>
          <h3 className="font-medium">Rendered file</h3>
          <pre className="max-h-64 overflow-auto rounded-md border bg-card p-3 font-mono text-xs leading-5">{file.next_content}</pre>
          <h3 className="font-medium">On disk vs next sync</h3>
          {file.differs ? <DiffView diff={file.diff} label="Instruction diff" className="max-h-64" />
            : <p className="text-muted-foreground">Matches the file on disk.</p>}
        </Tabs.Panel>)}
      </Tabs.Root>}
    </DialogContent>
  </Dialog>;
}

const actionKinds = ['create', 'update', 'remove', 'conflict'] as const;

export function SyncDialog({ onClose, changed }: { onClose: () => void; changed: () => void }) {
  const preview = useResource(api.previewSync, 0);
  const apply = useMutation();
  const [replanned, setReplanned] = useState(false);
  const actions = preview.data?.actions;
  const changedPlacement = preview.data?.changed;
  const completed = preview.data?.completed?.length ?? 0;
  return <Dialog open onOpenChange={open => { if (!open && !apply.pending) onClose(); }}>
    <DialogContent showCloseButton={!apply.pending} className="sm:max-w-2xl">
      <DialogHeader><DialogTitle>Sync now</DialogTitle>
        <DialogDescription>Review the changes before syncing. Resolve any remaining conflicts in Conflicts.</DialogDescription></DialogHeader>
      <ErrorNotice error={preview.error} />
      {preview.loading && <Loading rows={2} />}
      {replanned && <p role="status">{changedPlacement
        ? `${completed} ${completed === 1 ? 'change was' : 'changes were'} applied, then ${changedPlacement.name} changed — review the remaining plan.`
        : 'The sync plan changed. Review the new plan before applying.'}</p>}
      {!!preview.data?.errors?.length && <ul aria-label="Placement errors" className="text-destructive">
        {preview.data.errors.map((error, index) => <li key={index}>{error}</li>)}
      </ul>}
      {actions && <>
        <dl className="grid grid-cols-4 gap-2">
          {actionKinds.map(kind => <div key={kind} className="rounded-md border p-2">
            <dt className="capitalize text-muted-foreground">{kind}</dt>
            <dd className="font-medium tabular-nums">{actions.filter(action => action.kind === kind).length}</dd>
          </div>)}
        </dl>
        {actions.length ? <ul className="max-h-72 divide-y overflow-auto rounded-md border">
          {actions.map(action => <li key={JSON.stringify([action.harness, action.item, action.dest])} className="space-y-1 p-3">
            <p><span className="capitalize font-medium">{action.kind}</span> {action.item} {action.name} in {action.harness}</p>
            <p className="break-all font-mono text-xs text-muted-foreground">{action.dest}</p>
            {action.kind === 'conflict' && <p className="text-muted-foreground">Review in Conflicts.</p>}
          </li>)}
        </ul> : <p>No changes to sync.</p>}
      </>}
      <DialogFooter>
        <Button variant="outline" disabled={apply.pending} onClick={onClose}>Cancel</Button>
        <Button disabled={preview.loading || !preview.data?.fingerprint || !actions?.some(action => action.kind !== 'conflict') || apply.pending}
          onClick={() => void apply.run(async () => {
            try {
              const result = await api.syncNow(preview.data!.fingerprint!);
              if (result.fingerprint) {
                preview.setData(result);
                setReplanned(true);
                if (result.changed) changed();
                return;
              }
              toast(result.errors?.length
                ? { title: 'Sync completed with errors', description: result.errors.join('\n'), tone: 'destructive' }
                : { title: 'Sync complete', description: result.conflicts.length ? `${result.conflicts.length} ${result.conflicts.length === 1 ? 'conflict' : 'conflicts'} remaining.` : undefined });
            } catch (error) {
              toast({ title: 'Sync failed', description: errorMessage(error), tone: 'destructive' });
              return;
            }
            changed();
            onClose();
          })}><Pending pending={apply.pending}>Apply</Pending></Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>;
}
