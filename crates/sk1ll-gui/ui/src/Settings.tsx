import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Field, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { Action, Confirm, ErrorNotice, Loading, Notice, Pending } from './shared';
import type { StatusView } from './types';

export function Settings({ status, socket, revision, changed }: {
  status: StatusView; socket: string; revision: number; changed: () => void;
}) {
  const harnesses = useResource(api.harnesses, revision);
  const fleet = useResource(api.fleet, revision);
  const [hub, setHub] = useState(status.hub_url);
  const [code, setCode] = useState('');
  const [id, setId] = useState(status.machine_id);
  const [copyFrom, setCopyFrom] = useState('');
  const [joinCode, setJoinCode] = useState<string>();
  const [joined, setJoined] = useState('');
  const join = useMutation(changed);
  const createCode = useMutation();
  return <><h2>Harnesses on this Machine</h2><ErrorNotice error={harnesses.error} />
    {harnesses.loading && !harnesses.data && <Loading />}<Table><TableHeader><TableRow>
      {['Harness', 'Skills', 'Fragments', 'Mode', 'Origin'].map(label => <TableHead key={label}>{label}</TableHead>)}
    </TableRow></TableHeader><TableBody>{harnesses.data?.map(h => <TableRow key={h.name}>
      <TableCell>{h.name}</TableCell><TableCell><code>{h.harness.skills ?? 'None'}</code></TableCell>
      <TableCell><code>{h.harness.instructions ?? 'None'}</code><p className="meta">{h.harness.instructions_format}</p></TableCell>
      <TableCell>{h.harness.mode}</TableCell><TableCell><Badge variant="secondary">{h.origin}</Badge></TableCell>
    </TableRow>)}</TableBody></Table><p className="meta">Harnesses are configured in this Machine's machine.toml.</p>
    <h2>Join / rejoin</h2><p className="description">Join uses a single-use join code. Rejoin replaces this Machine's credentials and restarts its Daemon.</p>
    <form onSubmit={e => {
      e.preventDefault(); void join.run(() => api.join({ hub_url: hub, code, machine_id: id || undefined,
        copy_from: copyFrom || undefined }), value => { setCode(''); setJoined(`Joined as ${value.machine_id}. The Daemon is restarting.`); });
    }}><FieldGroup className="settings-form">
      <Field><FieldLabel htmlFor="hub-url">Hub URL</FieldLabel><Input id="hub-url" type="url" required value={hub} disabled={join.pending}
        onChange={e => setHub(e.target.value)} placeholder="https://skills.example.com" /></Field>
      <Field><FieldLabel htmlFor="join-code">Join code</FieldLabel><Input id="join-code" autoComplete="off" type="password" required value={code}
        disabled={join.pending} onChange={e => setCode(e.target.value)} /></Field>
      <Field><FieldLabel htmlFor="machine-id">Machine id (optional)</FieldLabel><Input id="machine-id" value={id} disabled={join.pending}
        onChange={e => setId(e.target.value)} /></Field>
      <Field><FieldLabel htmlFor="copy-from">Copy Assignment from Machine (optional)</FieldLabel><Input id="copy-from" value={copyFrom}
        disabled={join.pending} onChange={e => setCopyFrom(e.target.value)} /></Field>
      <div><Button type="submit" disabled={join.pending}><Pending pending={join.pending}>Join / rejoin</Pending></Button></div>
      <ErrorNotice error={join.error} />{joined && <Notice title="Join complete">{joined}</Notice>}
    </FieldGroup></form>
    <h2>Join code</h2><p className="description">Create a single-use code for another Machine. The code is shown once.</p>
    <div><Button variant="outline" disabled={createCode.pending} onClick={() => void createCode.run(api.joinCode, value => setJoinCode(value.code))}>
      <Pending pending={createCode.pending}>Create join code</Pending></Button></div><ErrorNotice error={createCode.error} />
    <Dialog open={joinCode !== undefined} onOpenChange={open => { if (!open) setJoinCode(undefined); }}>
      <DialogContent><DialogHeader><DialogTitle>Join code</DialogTitle><DialogDescription>Copy this code now. It cannot be shown again.</DialogDescription></DialogHeader>
        <code className="join-code">{joinCode}</code><Action label="Copy code" action={() => navigator.clipboard.writeText(joinCode ?? '')} />
      </DialogContent></Dialog>
    <h2>Remove a Machine</h2><p className="description">Removing a Machine revokes its credential. It must rejoin before its Daemon manages Harnesses again.</p>
    <ErrorNotice error={fleet.error} />{fleet.data?.machines.map(machine => <div className="removal row" key={machine.id}>
      <code>{machine.id}</code><Confirm label="Remove Machine" title={`Remove ${machine.id} from the Fleet?`} typed={machine.id}
        description="This revokes the Machine's credential and removes its Assignment. Content in the Baseline is kept."
        action={() => api.removeMachine(machine.id)} onDone={changed} /></div>)}
    <h2>Daemon connection</h2><dl className="report"><dt>Socket</dt><dd><code>{socket}</code></dd>
      <dt>Machine</dt><dd>{status.machine_id}</dd><dt>Hub</dt><dd>{status.hub_url}</dd>
      <dt>Hub connection</dt><dd>{status.connection.replaceAll('_', ' ')}</dd>
      <dt>Repo</dt><dd><code>{status.repo}</code></dd><dt>Assignment revision</dt><dd>{status.assignment_revision}</dd>
      <dt>Pending Reset reports</dt><dd>{status.pending_discards}</dd>
      <dt>Authentication rejected since</dt><dd>{status.auth_rejected_since === null ? 'Never' : new Date(status.auth_rejected_since * 1000).toLocaleString()}</dd>
    </dl>{status.last_error && <ErrorNotice error={status.last_error} />}
  </>;
}
