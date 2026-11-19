import { version as appVersion } from '../package.json';
import { useCallback, useRef, useState } from 'react';
import { LoaderCircle, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Collapsible, CollapsibleTrigger, CollapsibleContent } from '@/components/ui/collapsible';
import { NativeSelect } from '@/components/ui/native-select';
import { time } from './model';
import { toast } from './toast';
import { InstructionsDialog } from './SyncDialogs';
import { HarnessPanel, Path } from './HarnessSettings';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { Action, Details, ErrorNotice, Loading, Pending, Panel, PanelToolbar, SectionTitle, When } from './shared';
import type { FleetView, JoinCode, Scope, StatusView } from './types';

// What each scope lets the joining Machine do.
export const scopeSummary: Record<Scope, string> = {
  fleet: 'Can create join codes, change any Machine\'s Assignment, undo any log entry, and remove any Machine.',
  self: 'Can change only its own Assignment, undo log entries for itself, and remove itself. It cannot create join codes.',
};
// The scope this Machine joined with; undefined until the Fleet view loads.
export const ownScope = (fleet: FleetView | undefined, machineId: string): Scope | undefined => {
  const row = fleet?.machines.find(machine => machine.id === machineId);
  return row ? row.scope ?? 'fleet' : undefined;
};

// Creates a single-use join code of the chosen scope and shows it once. A self-only Machine can't create codes.
export function JoinCodeButton({ label, serverUrl, variant = 'outline', scope: own }: { label: string; serverUrl: string | null; variant?: 'outline' | 'default'; scope?: Scope }) {
  const [joinCode, setJoinCode] = useState<JoinCode>();
  const [scope, setScope] = useState<Scope>('fleet');
  const quote = (arg: string) => /^[a-zA-Z0-9_:/.-]+$/.test(arg) ? arg : "'" + arg.replaceAll("'", "'\\''") + "'";
  const command = serverUrl && joinCode ? `sk1ll join ${quote(serverUrl)} ${quote(joinCode.code)}` : null;
  const createCode = useMutation();
  const trigger = useRef<HTMLButtonElement>(null);
  if (own === 'self') return <p className="text-xs text-muted-foreground">This Machine joined with a Self-only join code, so it can manage only itself and cannot create join codes. Ask a Machine with Fleet scope, or the Server admin.</p>;
  const id = `join-scope-${label.replaceAll(' ', '-').toLowerCase()}`;
  return <div className="flex flex-col items-start gap-3">
    <div className="flex flex-wrap items-center gap-2">
      <NativeSelect id={id} aria-label="New Machine may manage" value={scope} disabled={createCode.pending} onChange={e => setScope(e.target.value as Scope)}>
        <option value="fleet">Whole Fleet</option><option value="self">Only itself</option></NativeSelect>
      <Button ref={trigger} variant={variant} className="relative" aria-label={label} aria-busy={createCode.pending} disabled={createCode.pending} onClick={() => void createCode.run(() => api.joinCode(scope), setJoinCode)}>
        {/* Keep the label's width while pending so the header doesn't reflow. */}
        <span className={createCode.pending ? 'invisible' : undefined}>{label}</span>
        {createCode.pending && <LoaderCircle aria-hidden className="absolute inset-0 m-auto animate-spin" />}</Button></div><ErrorNotice error={createCode.error} />
    <p className="text-xs text-muted-foreground">{scope === 'fleet' ? 'Whole Fleet' : 'Only itself'}: the new Machine {scopeSummary[scope].charAt(0).toLowerCase() + scopeSummary[scope].slice(1)}</p>
    <Dialog open={joinCode !== undefined} onOpenChange={open => { if (!open) setJoinCode(undefined); }}>
      <DialogContent finalFocus={trigger}><DialogHeader><DialogTitle>Join code</DialogTitle>
        <DialogDescription>Copy this single-use code now. It cannot be shown again and {joinCode?.expires_at != null ? <>expires {time(joinCode.expires_at)}.</> : <>expires after 24 hours by default. The Server operator can change the expiry with SK1LL_SERVER_JOIN_CODE_TTL.</>}
          {' '}On the new Machine, enter the code under Settings → Join / rejoin this Machine{command ? ', or run the command below.' : '.'}</DialogDescription></DialogHeader>
        <code className="rounded-md border bg-muted px-3 py-2 font-mono text-base break-all select-all">{joinCode?.code}</code>
        <p className="text-sm text-muted-foreground"><span className="font-medium text-foreground">{(joinCode?.scope ?? 'fleet') === 'fleet' ? 'Whole Fleet' : 'Only itself'}.</span> {scopeSummary[joinCode?.scope ?? 'fleet']}</p>
        <Action className="h-8" label="Copy code" doneLabel="Copied" action={() => navigator.clipboard.writeText(joinCode?.code ?? '')} />
        {command && <><code className="rounded-md border bg-muted px-3 py-2 font-mono text-sm break-all select-all">{command}</code>
          <Action className="h-8" label="Copy command" doneLabel="Copied" action={() => navigator.clipboard.writeText(command)} /></>}
      </DialogContent></Dialog>
  </div>;
}
export function Settings({ revision, fleetRevision = revision, status, socket, changed }: {
  status: StatusView; socket: string; fleetRevision?: number; revision: number; changed: () => void;
}) {
  const [previewHarness, setPreviewHarness] = useState<string>();
  const standalone = status.mode === 'standalone';
  const diagnostics: [string, string | number][] = [
    ['Mode', standalone ? 'Standalone' : 'Joined'], ['App version', appVersion],
    ...(status.version ? [['Daemon version', status.version] as [string, string]] : []),
    ['Repository', status.repo], ['Origin', status.has_origin ? 'Configured' : 'None'],
    ['Socket', socket], ['Assignment revision', status.assignment_revision],
  ];
  if (status.pending_discards > 0) diagnostics.push(['Pending Restore reports', status.pending_discards]);
  if (status.auth_rejected_since !== null) diagnostics.push(['Auth rejected since', time(status.auth_rejected_since)]);
  const fleet = useResource(api.fleet, fleetRevision, undefined, !standalone);
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
      ]} /></div>
      <HarnessPanel revision={revision} editorId={preferred ?? editorData.data?.preferred_editor} changed={changed}
        actions={h => h.enabled && h.harness.instructions && <Button size="sm" variant="ghost" onClick={() => setPreviewHarness(h.name)}>Preview instructions</Button>} />
      {previewHarness && <InstructionsDialog harness={previewHarness} onClose={() => setPreviewHarness(undefined)} />}
    </Panel>
    <Panel label="Fleet membership"><PanelToolbar><SectionTitle>Fleet membership</SectionTitle></PanelToolbar>
      {!standalone && <div className="border-b p-3.5"><h3 className="mb-1 text-sm font-medium">Join code for another Machine</h3><p className="mb-3 text-sm text-muted-foreground">Create a single-use code for another Machine. The code is shown once. Remove Machines from the Fleet screen.</p><JoinCodeButton label="Create join code" serverUrl={status.server_url} scope={ownScope(fleet.data, status.machine_id)} /></div>}
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
    <Panel label="Diagnostics"><PanelToolbar><SectionTitle>Diagnostics</SectionTitle>
      <Action className="ml-auto" label="Copy diagnostics" doneLabel="Copied" action={() => navigator.clipboard.writeText(
        [...diagnostics, ...(status.last_error ? [['Last error', status.last_error]] : [])].map(([label, value]) => `${label}: ${value}`).join('\n')
      )} /></PanelToolbar><div className="p-3.5"><Details rows={diagnostics.map(([label, value]) => [label,
        label === 'Socket' ? <code className="font-mono text-xs break-all">{value}</code>
          : label === 'Auth rejected since' ? <When seconds={status.auth_rejected_since} /> : value,
      ])} />{status.last_error && <div className="mt-3"><ErrorNotice error={status.last_error} /></div>}</div></Panel>

  </>;
}
