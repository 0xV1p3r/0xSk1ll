import { useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { Action, Confirm, Details, ErrorNotice, Loading, Notice, Pending } from './shared';
import type { StatusView } from './types';

// Creates a single-use join code and shows it once.
export function JoinCodeButton({ label, variant = 'outline' }: { label: string; variant?: 'outline' | 'default' }) {
  const [joinCode, setJoinCode] = useState<string>();
  const createCode = useMutation();
  const trigger = useRef<HTMLButtonElement>(null);
  return <div className="flex flex-col items-start gap-3">
    <Button ref={trigger} variant={variant} disabled={createCode.pending} onClick={() => void createCode.run(api.joinCode, value => setJoinCode(value.code))}>
      <Pending pending={createCode.pending}>{label}</Pending></Button><ErrorNotice error={createCode.error} />
    <Dialog open={joinCode !== undefined} onOpenChange={open => { if (!open) setJoinCode(undefined); }}>
      <DialogContent finalFocus={trigger}><DialogHeader><DialogTitle>Join code</DialogTitle>
        <DialogDescription>Copy this code now. It cannot be shown again. On the new Machine, enter it under Settings → Join / rejoin this Machine,
          or run <code className="font-mono">sk1ll join</code>.</DialogDescription></DialogHeader>
        <code className="rounded-md border bg-muted px-3 py-2 font-mono text-base break-all select-all">{joinCode}</code>
        <Action label="Copy code" doneLabel="Copied" action={() => navigator.clipboard.writeText(joinCode ?? '')} />
      </DialogContent></Dialog>
  </div>;
}
export function Settings({ status, socket, revision, changed }: {
  status: StatusView; socket: string; revision: number; changed: () => void;
}) {
  const harnesses = useResource(api.harnesses, revision);
  const fleet = useResource(api.fleet, revision);
  const [server, setServer] = useState(status.server_url);
  const [code, setCode] = useState('');
  const [id, setId] = useState(status.machine_id);
  const [copyFrom, setCopyFrom] = useState('');
  const [joined, setJoined] = useState('');
  const [confirming, setConfirming] = useState(false);
  const join = useMutation(changed);
  const connection = status.connection.replaceAll('_', ' ');
  return <>
    <Card><CardHeader><CardTitle>Harnesses on this Machine</CardTitle>
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

    <div className="grid items-start gap-6 xl:grid-cols-2">
      <Card><CardHeader><CardTitle>Join / rejoin this Machine</CardTitle>
        <CardDescription>Join uses a single-use join code. Rejoin replaces this Machine's credentials and restarts its Daemon.</CardDescription></CardHeader>
        <CardContent><form onSubmit={e => { e.preventDefault(); setConfirming(true); }}><FieldGroup>
          <Field><FieldLabel htmlFor="server-url">Server URL</FieldLabel><Input id="server-url" type="url" required aria-describedby={join.error ? 'join-error' : undefined} value={server} disabled={join.pending}
            onChange={e => setServer(e.target.value)} placeholder="https://skills.example.com" /></Field>
          <Field><FieldLabel htmlFor="join-code">Join code</FieldLabel><Input id="join-code" autoComplete="off" type="password" required aria-describedby={join.error ? 'join-error' : undefined} value={code}
            disabled={join.pending} onChange={e => setCode(e.target.value)} /></Field>
          <Field><FieldLabel htmlFor="machine-id">Machine id</FieldLabel><Input id="machine-id" value={id} disabled={join.pending}
            onChange={e => setId(e.target.value)} aria-describedby={join.error ? 'machine-id-hint join-error' : 'machine-id-hint'} /><FieldDescription id="machine-id-hint">Optional. Defaults to the id in machine.toml, else the hostname.</FieldDescription></Field>
          <Field><FieldLabel htmlFor="copy-from">Copy Assignment from Machine</FieldLabel><Input id="copy-from" value={copyFrom}
            disabled={join.pending} onChange={e => setCopyFrom(e.target.value)} placeholder="laptop" aria-describedby={join.error ? 'copy-from-hint join-error' : 'copy-from-hint'} />
            <FieldDescription id="copy-from-hint">Optional. Starts with another Machine's Assignment.</FieldDescription></Field>
          <FieldError id="join-error">{join.error}</FieldError>{joined && <Notice title="Join complete">{joined}</Notice>}
          <div><Button type="submit" disabled={join.pending}><Pending pending={join.pending}>Join / rejoin</Pending></Button></div>
        </FieldGroup></form>
        <Dialog open={confirming} onOpenChange={open => { if (!join.pending) setConfirming(open); }}>
          <DialogContent showCloseButton={!join.pending}><DialogHeader><DialogTitle>Join this Machine to the Server?</DialogTitle>
            <DialogDescription>This replaces {status.machine_id}'s credentials and restarts its Daemon.</DialogDescription></DialogHeader>
            <Details rows={[['Server', <span className="break-all">{server}</span>], ['Machine id', id || 'Default from machine.toml'],
              ['Assignment', copyFrom ? `Copied from ${copyFrom}` : 'Not copied']]} />
            <ErrorNotice error={join.error} /><DialogFooter>
              <Button variant="outline" disabled={join.pending} onClick={() => setConfirming(false)}>Cancel</Button>
              <Button disabled={join.pending} onClick={() => void join.run(() => api.join({ server_url: server, code, machine_id: id || undefined,
                copy_from: copyFrom || undefined }), value => {
                setCode(''); setConfirming(false); setJoined(`Joined as ${value.machine_id}. The Daemon is restarting.`);
              })}><Pending pending={join.pending}>Join / rejoin</Pending></Button>
            </DialogFooter></DialogContent></Dialog>
        </CardContent></Card>

      <div className="flex flex-col gap-6">
        <Card><CardHeader><CardTitle>Join code</CardTitle>
          <CardDescription>Create a single-use code for another Machine. The code is shown once.</CardDescription></CardHeader>
          <CardContent><JoinCodeButton label="Create join code" /></CardContent></Card>

        <Card className="@container"><CardHeader><CardTitle>Daemon connection</CardTitle></CardHeader>
          <CardContent><Details className="grid-cols-1 gap-y-1 [&_dd]:mb-3 @min-[28rem]:grid-cols-[minmax(0,10rem)_minmax(0,1fr)] @min-[28rem]:gap-y-2 @min-[28rem]:[&_dd]:mb-0" rows={[
            ['Socket', <code className="font-mono text-xs break-all">{socket}</code>],
            ['Machine', status.machine_id], ['Server', status.server_url], ['Server connection', <span className="capitalize">{connection}</span>],
            ['Repo', <code className="font-mono text-xs break-all">{status.repo}</code>], ['Assignment revision', status.assignment_revision],
            ['Pending Restore reports', status.pending_discards],
            ['Auth rejected since', status.auth_rejected_since === null ? 'Never' : new Date(status.auth_rejected_since * 1000).toLocaleString()],
          ]} />{status.last_error && <div className="mt-3"><ErrorNotice error={status.last_error} /></div>}</CardContent></Card>
      </div>
    </div>

    <Card className="ring-destructive/30"><CardHeader><CardTitle>Remove a Machine</CardTitle>
      <CardDescription>Removing a Machine revokes its credential. It must rejoin before its Daemon manages Harnesses again.</CardDescription></CardHeader>
      <CardContent className="flex flex-col gap-3"><ErrorNotice error={fleet.error} />
        {fleet.loading && !fleet.data && <Loading rows={2} />}
        <div className="divide-y rounded-lg border">{fleet.data?.machines.map(machine => <div className="flex items-center justify-between gap-4 px-4 py-2" key={machine.id}>
          <span className="flex items-center gap-2"><code className="font-mono text-sm">{machine.id}</code>
            {machine.id === status.machine_id && <Badge variant="secondary">This Machine</Badge>}</span>
          <Confirm label="Remove Machine" ariaLabel={`Remove ${machine.id} from the Fleet`} title={`Remove ${machine.id} from the Fleet?`} typed={machine.id}
            description="This revokes the Machine's credential and removes its Assignment. Content in the Baseline is kept."
            action={() => api.removeMachine(machine.id)} onDone={changed} /></div>)}</div>
      </CardContent></Card>

  </>;
}
