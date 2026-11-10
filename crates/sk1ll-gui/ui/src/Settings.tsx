import { useCallback, useRef, useState } from 'react';
import { LoaderCircle, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Collapsible, CollapsibleTrigger, CollapsibleContent } from '@/components/ui/collapsible';
import { NativeSelect } from '@/components/ui/native-select';
import { toast } from './toast';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { Action, Details, ErrorNotice, Loading, Pending } from './shared';
import type { StatusView } from './types';

// Creates a single-use join code and shows it once.
export function JoinCodeButton({ label, variant = 'outline' }: { label: string; variant?: 'outline' | 'default' }) {
  const [joinCode, setJoinCode] = useState<string>();
  const createCode = useMutation();
  const trigger = useRef<HTMLButtonElement>(null);
  return <div className="flex flex-col items-start gap-3">
    <Button ref={trigger} variant={variant} className="relative" aria-label={label} aria-busy={createCode.pending} disabled={createCode.pending} onClick={() => void createCode.run(api.joinCode, value => setJoinCode(value.code))}>
      {/* Keep the label's width while pending so the header doesn't reflow. */}
      <span className={createCode.pending ? 'invisible' : undefined}>{label}</span>
      {createCode.pending && <LoaderCircle aria-hidden className="absolute inset-0 m-auto animate-spin" />}</Button><ErrorNotice error={createCode.error} />
    <Dialog open={joinCode !== undefined} onOpenChange={open => { if (!open) setJoinCode(undefined); }}>
      <DialogContent finalFocus={trigger}><DialogHeader><DialogTitle>Join code</DialogTitle>
        <DialogDescription>Copy this code now. It cannot be shown again. On the new Machine, enter it under Settings → Join / rejoin this Machine,
          or run <code className="font-mono">sk1ll join</code>.</DialogDescription></DialogHeader>
        <code className="rounded-md border bg-muted px-3 py-2 font-mono text-base break-all select-all">{joinCode}</code>
        <Action className="h-8" label="Copy code" doneLabel="Copied" action={() => navigator.clipboard.writeText(joinCode ?? '')} />
      </DialogContent></Dialog>
  </div>;
}
export function Settings({ status, socket, revision, changed }: {
  status: StatusView; socket: string; revision: number; changed: () => void;
}) {
  const harnesses = useResource(api.harnesses, revision);
  const saveEditor = useMutation();
  const [preferred, setPreferred] = useState<string>();
  const [refresh, setRefresh] = useState(0);
  const loadEditors = useCallback(() => api.editors(refresh > 0), [refresh]);
  const editorData = useResource(loadEditors, revision);
  const [server, setServer] = useState(status.server_url);
  const [code, setCode] = useState('');
  const [id, setId] = useState(status.machine_id);
  const [copyFrom, setCopyFrom] = useState('');
  const alreadyJoined = !!status.server_url && status.connection !== 'auth_rejected' && !status.revoked;
  const [confirming, setConfirming] = useState(false);
  const join = useMutation(changed);
  const connection = status.connection.replaceAll('_', ' ');
  return <>
    <Card className="pt-0"><CardHeader className="border-b border-border bg-muted/40 py-3"><CardTitle className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">Harnesses on this Machine</CardTitle>
      <CardDescription>Harnesses are configured in this Machine's <code className="font-mono">machine.toml</code>.</CardDescription></CardHeader>
      <CardContent className="flex flex-col gap-3"><ErrorNotice error={harnesses.error} />
        {harnesses.loading && !harnesses.data && <Loading rows={2} />}
        {harnesses.data && <div className="overflow-hidden rounded-lg border"><Table aria-label="Harnesses on this Machine"><TableHeader><TableRow className="bg-muted/50 hover:bg-muted/50">
          {['Harness', 'Skills', 'Rules', 'Mode', 'Origin'].map(label => <TableHead key={label}>{label}</TableHead>)}
        </TableRow></TableHeader><TableBody>{harnesses.data.map(h => <TableRow key={h.name}>
          <TableCell className="font-medium">{h.name}</TableCell><TableCell><code className="font-mono text-xs">{h.harness.skills ?? 'None'}</code></TableCell>
          <TableCell><code className="font-mono text-xs">{h.harness.instructions ?? 'None'}</code>
            <p className="text-xs text-muted-foreground">{h.harness.instructions_format}</p></TableCell>
          <TableCell>{h.harness.mode}</TableCell><TableCell><Badge variant="secondary">{h.origin}</Badge></TableCell>
        </TableRow>)}</TableBody></Table></div>}
      </CardContent></Card>

    <Card className="pt-0"><CardHeader className="border-b border-border bg-muted/40 py-3"><CardTitle className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">Editor</CardTitle></CardHeader>
      <CardContent className="flex flex-col gap-3"><ErrorNotice error={editorData.error || saveEditor.error} />
        {editorData.loading && !editorData.data && <Loading rows={1} />}
        {!editorData.data && !editorData.loading && <div><Button variant="outline" onClick={() => { setPreferred(undefined); setRefresh(value => value + 1); }}><RefreshCw data-icon="inline-start" />Refresh editors</Button></div>}
        {editorData.data && <Field className="max-w-sm"><FieldLabel htmlFor="preferred-editor">Preferred editor</FieldLabel>
          <div className="flex items-center gap-2"><NativeSelect id="preferred-editor" className="w-full" value={preferred ?? editorData.data.preferred_editor} disabled={saveEditor.pending}
            onChange={e => { const value = e.target.value; void saveEditor.run(() => api.setPreferredEditor(value), () => { setPreferred(value); toast({ title: 'Preferred editor saved' }); }); }}>
            {(['gui', 'terminal', 'system'] as const).map(kind => <optgroup key={kind} label={{ gui: 'GUI', terminal: 'Terminal', system: 'System' }[kind]}>
              {editorData.data?.editors.filter(editor => editor.kind === kind).map(editor => <option key={editor.id} value={editor.id}>{editor.name}</option>)}</optgroup>)}
          </NativeSelect>
          <Button variant="outline" size="icon" aria-label="Refresh editors" title="Refresh editors" disabled={editorData.loading} onClick={() => { setPreferred(undefined); setRefresh(value => value + 1); }}><RefreshCw /></Button></div></Field>}
      </CardContent></Card>

        <Card className="@container pt-0"><CardHeader className="border-b border-border bg-muted/40 py-3"><CardTitle className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">Daemon connection</CardTitle></CardHeader>
          <CardContent><Details className="grid-cols-1 gap-y-1 [&_dd]:mb-3 @min-[28rem]:grid-cols-[minmax(0,10rem)_minmax(0,1fr)] @min-[28rem]:gap-y-2 @min-[28rem]:[&_dd]:mb-0" rows={[
            ['Socket', <code className="font-mono text-xs break-all">{socket}</code>],
            ['Machine', status.machine_id], ['Server', status.server_url], ['Server connection', <span className="capitalize">{connection}</span>],
            ['Repo', <code className="font-mono text-xs break-all">{status.repo}</code>], ['Assignment revision', status.assignment_revision],
            ['Pending Restore reports', status.pending_discards],
            ['Auth rejected since', status.auth_rejected_since === null ? 'Never' : new Date(status.auth_rejected_since * 1000).toLocaleString()],
          ]} />{status.last_error && <div className="mt-3"><ErrorNotice error={status.last_error} /></div>}</CardContent></Card>
    <Card className="pt-0"><CardHeader className="border-b border-border bg-muted/40 py-3"><CardTitle className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">Join code</CardTitle>
      <CardDescription>Create a single-use code for another Machine. The code is shown once. Remove Machines from the Fleet screen.</CardDescription></CardHeader>
      <CardContent><JoinCodeButton label="Create join code" /></CardContent></Card>
      <Card className="pt-0"><CardHeader className="border-b border-border bg-muted/40 py-3"><CardTitle className="text-[11px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">Join / rejoin this Machine</CardTitle>
        <CardDescription>Join uses a single-use join code. Rejoin replaces this Machine's credentials and restarts its Daemon.</CardDescription></CardHeader>
        <CardContent><Collapsible key={String(alreadyJoined)} defaultOpen={!alreadyJoined}>
          {alreadyJoined && <><p className="mb-3 text-sm text-muted-foreground">This Machine is joined. Rejoin to replace its credentials.</p><CollapsibleTrigger render={<Button variant="outline" />}>Rejoin this Machine…</CollapsibleTrigger></>}
          <CollapsibleContent><form onSubmit={e => { e.preventDefault(); setConfirming(true); }}><FieldGroup>
          <Field><FieldLabel htmlFor="server-url">Server URL</FieldLabel><Input id="server-url" type="url" required aria-describedby={join.error ? 'join-error' : undefined} value={server} disabled={join.pending}
            onChange={e => setServer(e.target.value)} placeholder="https://skills.example.com" /></Field>
          <Field><FieldLabel htmlFor="join-code">Join code</FieldLabel><Input id="join-code" autoComplete="off" type="password" required aria-describedby={join.error ? 'join-error' : undefined} value={code}
            disabled={join.pending} onChange={e => setCode(e.target.value)} /></Field>
          <Field><FieldLabel htmlFor="machine-id">Machine id</FieldLabel><Input id="machine-id" value={id} disabled={join.pending}
            onChange={e => setId(e.target.value)} aria-describedby={join.error ? 'machine-id-hint join-error' : 'machine-id-hint'} /><FieldDescription id="machine-id-hint">Optional. Defaults to the id in machine.toml, else the hostname.</FieldDescription></Field>
          <Field><FieldLabel htmlFor="copy-from">Copy Assignment from Machine</FieldLabel><Input id="copy-from" value={copyFrom}
            disabled={join.pending} onChange={e => setCopyFrom(e.target.value)} placeholder="laptop" aria-describedby={join.error ? 'copy-from-hint join-error' : 'copy-from-hint'} />
            <FieldDescription id="copy-from-hint">Optional. Starts with another Machine's Assignment.</FieldDescription></Field>
          <FieldError id="join-error">{join.error}</FieldError>
          <div><Button type="submit" disabled={join.pending}><Pending pending={join.pending}>Join / rejoin</Pending></Button></div>
        </FieldGroup></form></CollapsibleContent></Collapsible>
        <Dialog open={confirming} onOpenChange={open => { if (!join.pending) setConfirming(open); }}>
          <DialogContent showCloseButton={!join.pending}><DialogHeader><DialogTitle>Join this Machine to the Server?</DialogTitle>
            <DialogDescription>This replaces {status.machine_id}'s credentials and restarts its Daemon.</DialogDescription></DialogHeader>
            <Details rows={[['Server', <span className="break-all">{server}</span>], ['Machine id', id || 'Default from machine.toml'],
              ['Assignment', copyFrom ? `Copied from ${copyFrom}` : 'Not copied']]} />
            <ErrorNotice error={join.error} /><DialogFooter>
              <Button variant="outline" disabled={join.pending} onClick={() => setConfirming(false)}>Cancel</Button>
              <Button disabled={join.pending} onClick={() => void join.run(() => api.join({ server_url: server, code, machine_id: id || undefined,
                copy_from: copyFrom || undefined }), value => {
                setCode(''); setConfirming(false); toast({ title: 'Join complete', description: `Joined as ${value.machine_id}. The Daemon is restarting.` });
              })}><Pending pending={join.pending}>Join / rejoin</Pending></Button>
            </DialogFooter></DialogContent></Dialog>
        </CardContent></Card>

  </>;
}
