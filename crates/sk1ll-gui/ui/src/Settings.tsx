import { useCallback, useRef, useState } from 'react';
import { LoaderCircle, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { Collapsible, CollapsibleTrigger, CollapsibleContent } from '@/components/ui/collapsible';
import { NativeSelect } from '@/components/ui/native-select';
import { toast } from './toast';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { Action, Details, ErrorNotice, Loading, Pending, Panel, PanelToolbar, SectionTitle, When } from './shared';
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
export function Settings({ revision, fleetRevision = revision, status, socket, changed }: {
  status: StatusView; socket: string; fleetRevision?: number; revision: number; changed: () => void;
}) {
  const standalone = status.mode === 'standalone';
  const fleet = useResource(api.fleet, fleetRevision, undefined, !standalone);
  const harnesses = useResource(api.harnesses, revision);
  const saveEditor = useMutation();
  const [preferred, setPreferred] = useState<string>();
  const [refresh, setRefresh] = useState(0);
  const loadEditors = useCallback(() => api.editors(refresh > 0), [refresh]);
  const editorData = useResource(loadEditors, revision);
  const [server, setServer] = useState(status.server_url ?? '');
  const [code, setCode] = useState('');
  const [id, setId] = useState('');
  const [showCode, setShowCode] = useState(false);
  // The admin token creates a join code for the first Machine of a new Fleet.
  const [credential, setCredential] = useState<'code' | 'admin'>('code');
  const secretLabel = credential === 'admin' ? 'Admin token' : 'Join code';
  const [copyFrom, setCopyFrom] = useState('');
  const alreadyJoined = !!status.server_url && status.connection !== 'auth_rejected' && !status.revoked;
  const [confirming, setConfirming] = useState(false);
  const join = useMutation(changed);
  const connection = status.connection.replaceAll('_', ' ');
  return <>
    <Panel label="This machine"><PanelToolbar><SectionTitle>This machine</SectionTitle></PanelToolbar>
      <div className="p-3.5"><Details rows={[
        ['Machine id', status.machine_id], ['Server', status.server_url || 'Not configured'], ['Connection', <span className="capitalize">{connection}</span>],
        ['Repo', <Path value={status.repo} />],
      ]} /></div><div className="px-3.5"><h3 className="text-sm font-medium">Harnesses on this Machine</h3><p className="text-sm text-muted-foreground">Harnesses are configured in this Machine's <code className="font-mono">machine.toml</code>.</p></div>
      <div className="flex flex-col gap-3 p-3.5"><ErrorNotice error={harnesses.error} />
        {harnesses.loading && !harnesses.data && <Loading rows={2} />}
        {harnesses.data && <div className="overflow-hidden rounded-lg border"><Table aria-label="Harnesses on this Machine"><TableHeader><TableRow className="bg-muted/50 hover:bg-muted/50">
          {['Harness', 'Skills', 'Rules', 'Mode', 'Origin'].map(label => <TableHead key={label}>{label}</TableHead>)}
        </TableRow></TableHeader><TableBody>{harnesses.data.map(h => <TableRow key={h.name}>
          <TableCell className="font-medium">{h.name}</TableCell><TableCell><Path value={h.harness.skills} /></TableCell>
          <TableCell><Path value={h.harness.instructions} />
            <p className="text-xs text-muted-foreground">{h.harness.instructions && (h.harness.instructions_format === 'block' ? 'Single file' : 'Rule files')}</p></TableCell>
          <TableCell>{h.harness.mode === 'symlink' ? 'Symbolic link' : 'Copy'}</TableCell><TableCell><Badge variant="secondary">{h.origin === 'detected' ? 'Detected' : h.origin === 'machine.toml' ? 'Machine configuration' : h.origin.replaceAll('_', ' ')}</Badge></TableCell>
        </TableRow>)}</TableBody></Table></div>}
      </div>

    </Panel>
    <Panel label="Fleet membership"><PanelToolbar><SectionTitle>Fleet membership</SectionTitle></PanelToolbar>
      {!standalone && <div className="border-b p-3.5"><h3 className="mb-1 text-sm font-medium">Join code for another Machine</h3><p className="mb-3 text-sm text-muted-foreground">Create a single-use code for another Machine. The code is shown once. Remove Machines from the Fleet screen.</p><JoinCodeButton label="Create join code" /></div>}
      <div className="px-3.5 pt-3.5"><h3 className="text-sm font-medium">{standalone ? 'Join a Fleet' : 'Join / rejoin this Machine'}</h3><p className="text-sm text-muted-foreground">Join uses a single-use join code. Rejoin replaces this Machine's credentials and restarts its Daemon.</p></div>
      <div className="p-3.5"><Collapsible key={String(alreadyJoined)} defaultOpen={!alreadyJoined}>
          {alreadyJoined && <><p className="mb-3 text-sm text-muted-foreground">This Machine is joined. Rejoin to replace its credentials.</p><CollapsibleTrigger render={<Button variant="outline" />}>Rejoin this Machine…</CollapsibleTrigger></>}
          <CollapsibleContent><form onSubmit={e => { e.preventDefault(); setConfirming(true); }}><FieldGroup>
          <Field><FieldLabel htmlFor="server-url">Server URL</FieldLabel><Input id="server-url" type="url" required aria-describedby={join.error ? 'join-error' : undefined} value={server} disabled={join.pending}
            onChange={e => setServer(e.target.value)} placeholder="https://skills.example.com" /></Field>
          <Field><FieldLabel htmlFor="join-with">Join with</FieldLabel><NativeSelect id="join-with" value={credential} disabled={join.pending} onChange={e => setCredential(e.target.value as 'code' | 'admin')} aria-describedby="join-with-hint">
            <option value="code">Join code from another Machine</option><option value="admin">Server admin token</option></NativeSelect>
            <FieldDescription id="join-with-hint">{credential === 'admin' ? 'For the first Machine of a new Fleet. The token is used once to create a join code and is not saved.' : 'Create one on a joined Machine under Settings.'}</FieldDescription></Field>
          <Field><FieldLabel htmlFor="join-code">{secretLabel}</FieldLabel><div className="flex items-center gap-2"><Input id="join-code" autoComplete="off" type={showCode ? 'text' : 'password'} required aria-describedby={join.error ? 'join-error' : undefined} value={code}
            disabled={join.pending} onChange={e => setCode(e.target.value)} /><Button type="button" variant="outline" aria-label={`${showCode ? 'Hide' : 'Show'} ${secretLabel.toLowerCase()}`} aria-pressed={showCode} onClick={() => setShowCode(value => !value)}>{showCode ? 'Hide' : 'Show'}</Button></div></Field>
          <Field><FieldLabel htmlFor="machine-id">Machine id</FieldLabel><Input id="machine-id" placeholder={status.machine_id} value={id} disabled={join.pending}
            onChange={e => setId(e.target.value)} aria-describedby={join.error ? 'machine-id-hint join-error' : 'machine-id-hint'} /><FieldDescription id="machine-id-hint">Optional. Defaults to the current Machine id: {status.machine_id}.</FieldDescription></Field>
          <Field><FieldLabel htmlFor="copy-from">Copy Assignment from Machine</FieldLabel>{standalone || fleet.error ? <Input id="copy-from" value={copyFrom} disabled={join.pending} onChange={e => setCopyFrom(e.target.value)} placeholder="Machine id" aria-describedby="copy-from-hint" /> : <NativeSelect id="copy-from" value={copyFrom} disabled={join.pending || fleet.loading} onChange={e => setCopyFrom(e.target.value)} aria-describedby="copy-from-hint">
            <option value="">Don't copy</option>{fleet.data?.machines.map(machine => <option key={machine.id} value={machine.id}>{machine.id}</option>)}
          </NativeSelect>}
            <FieldDescription id="copy-from-hint">Optional. Starts with another Machine's Assignment.{fleet.error && ' Fleet could not be loaded. Enter a Machine id, or leave empty.'}</FieldDescription></Field>
          <FieldError id="join-error">{join.error}</FieldError>
          <div><Button type="submit" disabled={join.pending}><Pending pending={join.pending}>Join / rejoin</Pending></Button></div>
        </FieldGroup></form></CollapsibleContent></Collapsible>
        <Dialog open={confirming} onOpenChange={open => { if (!join.pending) setConfirming(open); }}>
          <DialogContent showCloseButton={!join.pending}><DialogHeader><DialogTitle>Join this Machine to the Server?</DialogTitle>
            <DialogDescription>This replaces {status.machine_id}'s credentials and restarts its Daemon.</DialogDescription></DialogHeader>
            <Details rows={[['Server', <span className="break-all">{server}</span>], ['Machine id', id || status.machine_id], ['Credential', secretLabel],
              ['Assignment', copyFrom ? `Copied from ${copyFrom}` : 'Not copied']]} />
            <ErrorNotice error={join.error} /><DialogFooter>
              <Button variant="outline" disabled={join.pending} onClick={() => setConfirming(false)}>Cancel</Button>
              <Button disabled={join.pending} onClick={() => void join.run(() => api.join({ server_url: server, code: credential === 'code' ? code : '',
                admin_token: credential === 'admin' ? code : undefined, machine_id: id || undefined,
                copy_from: copyFrom || undefined }), value => {
                setCode(''); setConfirming(false); toast({ title: 'Join complete', description: `Joined as ${value.machine_id}. The Daemon is restarting.` });
              })}><Pending pending={join.pending}>Join / rejoin</Pending></Button>
            </DialogFooter></DialogContent></Dialog>
        </div>
    </Panel>
    <Panel label="Editor"><PanelToolbar><SectionTitle>Editor</SectionTitle></PanelToolbar>
      <div className="flex flex-col gap-3 p-3.5"><ErrorNotice error={editorData.error || saveEditor.error} />
        {editorData.loading && !editorData.data && <Loading rows={1} />}
        {(!editorData.data || editorData.data.editors.length === 0) && !editorData.loading && <div>{editorData.data && <p className="mb-3 text-sm text-muted-foreground">No editors found</p>}<Button variant="outline" onClick={() => { setPreferred(undefined); setRefresh(value => value + 1); }}><RefreshCw data-icon="inline-start" />Refresh editors</Button></div>}
        {!!editorData.data?.editors.length && <Field className="max-w-sm"><FieldLabel htmlFor="preferred-editor">Preferred editor</FieldLabel>
          <div className="flex items-center gap-2"><NativeSelect id="preferred-editor" className="w-full" value={preferred ?? editorData.data.preferred_editor} disabled={saveEditor.pending}
            onChange={e => { const value = e.target.value; void saveEditor.run(() => api.setPreferredEditor(value), () => { setPreferred(value); toast({ title: 'Preferred editor saved' }); }); }}>
            {(['gui', 'terminal', 'system'] as const).map(kind => <optgroup key={kind} label={{ gui: 'GUI', terminal: 'Terminal', system: 'System' }[kind]}>
              {editorData.data?.editors.filter(editor => editor.kind === kind).map(editor => <option key={editor.id} value={editor.id}>{editor.name}</option>)}</optgroup>)}
          </NativeSelect>
          <Button variant="outline" size="icon" aria-label="Refresh editors" title="Refresh editors" disabled={editorData.loading} onClick={() => { setPreferred(undefined); setRefresh(value => value + 1); }}><RefreshCw /></Button></div></Field>}
      </div>

    </Panel>
    <Panel label="Diagnostics"><PanelToolbar><SectionTitle>Diagnostics</SectionTitle></PanelToolbar><div className="p-3.5"><Details rows={[
      ['Mode', standalone ? 'Standalone' : 'Joined'], ['Repository', status.repo], ['Origin', status.has_origin ? 'Configured' : 'None'],
      ['Socket', <code className="font-mono text-xs break-all">{socket}</code>], ['Assignment revision', status.assignment_revision],
      ...(status.pending_discards > 0 ? [['Pending Restore reports', status.pending_discards] as [string, number]] : []),
      ...(status.auth_rejected_since !== null ? [['Auth rejected since', <When seconds={status.auth_rejected_since} />] as [string, React.ReactNode]] : []),
    ]} />{status.last_error && <div className="mt-3"><ErrorNotice error={status.last_error} /></div>}</div></Panel>

  </>;
}

function Path({ value }: { value: string | null }) {
  return value ? <code className="font-mono text-xs break-all">{value}</code> : <span className="text-muted-foreground">Not configured</span>;
}
