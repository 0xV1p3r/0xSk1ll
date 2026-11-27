import { isTauri } from '@tauri-apps/api/core';
import { useEffect, useState, type ReactNode } from 'react';
import { ArrowLeft, Check, LoaderCircle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { ThemeSelector } from '@/components/theme-selector';
import wordmarkDark from '@branding/wordmark-dark.svg';
import wordmarkLight from '@branding/wordmark-light.svg';
import { cn } from '@/lib/utils';
import { api, errorMessage } from './api';
import { useMutation, useResource } from './hooks';
import { insecureHttp, plural } from './model';
import { JoinCodeButton } from './Settings';
import { Details, ErrorNotice, Loading, Notice, Panel, PanelToolbar, Pending, SectionTitle } from './shared';
import type { ContentEntry, DaemonStarted, HarnessCandidate, HarnessChoice, Scope, ServiceManager, StatusView } from './types';

export function Wordmark() {
  return <div className="mb-3 w-44"><img src={wordmarkLight} alt="0xSk1ll" className="block h-auto w-full dark:hidden" /><img src={wordmarkDark} alt="0xSk1ll" className="hidden h-auto w-full dark:block" /></div>;
}
export function CopyCommand({ command }: { command: string }) {
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => { if (!copied) return; const timer = setTimeout(() => setCopied(false), 2000); return () => clearTimeout(timer); }, [copied]);
  return <div><div className="flex items-center gap-2"><pre className="min-w-0 flex-1 overflow-auto rounded-md border bg-muted px-3 py-2 font-mono text-xs">{command}</pre><Button variant="outline" size="sm" aria-label={`Copy ${command}`} onClick={() => { void (async () => { try { await navigator.clipboard.writeText(command); setCopied(true); setError(''); } catch (e) { setError(errorMessage(e)); } })(); }}>{copied ? 'Copied' : 'Copy'}</Button></div><span role="status" className="sr-only">{copied ? 'Command copied' : ''}</span><ErrorNotice error={error} /></div>;
}
// Every pre-app screen: one centred card with the wordmark.
function Frame({ title, description, step, children, wide = false }: { title: string; description: ReactNode; step?: string; children: ReactNode; wide?: boolean }) {
  return <main className="relative grid min-h-svh place-items-center bg-muted/40 p-6">
    <div className="absolute top-4 right-4"><ThemeSelector /></div>
    <Card className={cn('w-full', wide ? 'max-w-xl' : 'max-w-lg')}>
      <CardHeader><Wordmark />{step && <p className="text-xs text-muted-foreground">{step}</p>}<CardTitle className="text-xl">{title}</CardTitle>
        <CardDescription>{description}</CardDescription></CardHeader>
      <CardContent className="flex flex-col gap-4 text-sm">{children}</CardContent>
    </Card>
  </main>;
}

// What to run by hand for each platform's user service; the background process logs to the cache dir.
const serviceCommands: Record<ServiceManager, { name: string; start: string; restart: string; log: string; processLog: string }> = {
  systemd: { name: 'systemd user service', start: 'systemctl --user start sk1ll-daemon', restart: 'systemctl --user restart sk1ll-daemon',
    log: 'journalctl --user -u sk1ll-daemon -n 50', processLog: 'tail -n 50 ~/.cache/sk1ll/daemon.log' },
};

export function ConnectionPage({ socket, error, apiMismatch = false }: { socket: string; error: string; apiMismatch?: boolean }) {
  // Re-read after a failed restart: the unit may have stopped.
  const [serviceRevision, setServiceRevision] = useState(0);
  const service = useResource(api.daemonService, serviceRevision);
  const start = useMutation();
  const [started, setStarted] = useState<DaemonStarted>();
  const [slow, setSlow] = useState(false);
  useEffect(() => { if (!started) return; const timer = setTimeout(() => setSlow(true), 15000); return () => clearTimeout(timer); }, [started]);
  const info = service.data;
  const commands = info && serviceCommands[info.manager];
  const serviceName = 'sk1ll-daemon user service';
  const how = !info?.binary ? '' : info.available ? info.unit ? `Enables and starts the ${serviceName}.`
    : `Installs the ${serviceName} and starts it. It starts again whenever you log in.`
    : 'Runs the Daemon in the background until you log out.';
  const log = started === 'process' ? commands?.processLog : commands?.log;
  if (apiMismatch) return <Frame title="Restart the Daemon" description="The app and the running Daemon use different local API versions. The connection retries automatically.">
    {info?.restartable
      ? <Button className="self-start" disabled={start.pending} onClick={() => void start.run(() => api.restartDaemon().finally(() => setServiceRevision(n => n + 1)))}><Pending pending={start.pending}>Restart daemon</Pending></Button>
      : !service.loading && <><p>If you started the Daemon in a terminal, stop it there and run:</p><CopyCommand command="sk1ll daemon" />
        {commands && <><p>For a {commands.name}, run:</p><CopyCommand command={commands.restart} /></>}</>}
    <ErrorNotice error={start.error || service.error || error} />
  </Frame>;
  return <Frame title="Daemon is not running" description="The Daemon keeps this Machine's Harnesses in step and serves this window. The connection retries automatically.">
    {info?.default_socket && info.binary && <div className="flex flex-col items-start gap-2">
      <Button disabled={start.pending || !!started} onClick={() => void start.run(api.startDaemon, setStarted)}>
        <Pending pending={start.pending}>{started ? <><Check data-icon="inline-start" />Daemon started</> : 'Start the Daemon'}</Pending></Button>
      <p className="text-xs text-muted-foreground">{how} <span className="font-mono break-all">{info.binary}</span></p>
      <div className="whitespace-pre-line"><ErrorNotice error={start.error} title="Could not start the Daemon" /></div></div>}
    {info?.default_socket && !info.binary && <Notice title="The sk1ll command is not installed">Install it, for example with <code className="font-mono">cargo install --path crates/sk1ll-cli</code> in the 0xSk1ll source, then start the Daemon here.</Notice>}
    {slow && log && <Notice title="The Daemon has not answered yet">Check its log for the reason:<div className="mt-2"><CopyCommand command={log} /></div></Notice>}
    <Collapsible defaultOpen={!info || !info.default_socket || !info.binary} key={String(!!info)}>
      <CollapsibleTrigger render={<Button variant="ghost" size="sm" className="-ml-2" />}>Start it yourself</CollapsibleTrigger>
      <CollapsibleContent className="flex flex-col gap-3 pt-2">
        {info && commands && <div className="flex flex-col gap-2"><span className="text-muted-foreground">Start the service</span>
          <CopyCommand command={commands.start} /></div>}
        <div className="flex flex-col gap-2"><span className="text-muted-foreground">Or run it in a terminal</span>
          <CopyCommand command="sk1ll daemon" /></div>
      </CollapsibleContent>
    </Collapsible>
    <p className="text-xs text-muted-foreground">Socket <code className="font-mono break-all">{socket}</code></p>
    <ErrorNotice error={error} />
    <Badge variant="secondary" role="status"><LoaderCircle className="animate-spin" />{started ? 'Waiting for the Daemon…' : 'Reconnecting…'}</Badge>
  </Frame>;
}

export function RestartingPage() {
  return <Frame title="Joining the Fleet" description="The Daemon is restarting with this Machine's Fleet credentials.">
    <Badge variant="secondary" role="status"><LoaderCircle className="animate-spin" />Restarting the Daemon…</Badge>
  </Frame>;
}

type SetupPath = 'local' | 'join' | 'create';
const setupPaths: { value: SetupPath; title: string; description: string }[] = [
  { value: 'local', title: 'Only this Machine', description: 'Keep Skills and Rules in a local repo. You can join a Fleet later in Settings.' },
  { value: 'join', title: 'Join a Fleet', description: 'Another Machine already syncs with a Server. Create a join code on it in Settings.' },
  { value: 'create', title: 'Join as the first Machine', description: 'Requires a deployed Server and its admin token.' },
];
export type SetupResult = { joined: false } | { joined: true; copied: boolean };

// First run: there is no skills repo yet, so the Daemon runs standalone.
export function SetupWizard({ status, onDone }: { status: StatusView; onDone: (result: SetupResult) => void }) {
  const [step, setStep] = useState<'path' | 'harnesses' | 'details'>('path');
  const docs = useMutation();
  // Owned here so Back and Continue wait for a Harness choice to save. A failed save stays visible but never traps the wizard.
  const harnessSave = useMutation();
  // Config folders this app's environment points at: kept ones are added as Harnesses on Continue.
  const [found, setFound] = useState<HarnessCandidate[]>([]);
  const [declined, setDeclined] = useState<string[]>([]);
  const kept = found.filter(c => !c.warning && !declined.includes(c.name));
  const continueToDetails = () => void harnessSave.run(async () => {
    for (const c of kept) await api.addSetupInstance(c.name, c.harness);
  }, () => setStep('details'));
  const [path, setPath] = useState<SetupPath>('local');
  if (step === 'path') return <Frame wide step="Step 1 of 3" title="Set up this Machine" description="0xSk1ll keeps your agent Skills and Rules in one git repo and syncs them into every Harness.">
    <fieldset className="flex flex-col gap-2"><legend className="sr-only">How this Machine is used</legend>
      {setupPaths.map(option => <label key={option.value} className="flex cursor-pointer gap-3 rounded-lg border p-3 has-checked:border-primary has-checked:bg-accent/50 has-focus-visible:ring-3 has-focus-visible:ring-ring">
        <input type="radio" name="setup-path" value={option.value} checked={path === option.value} onChange={() => setPath(option.value)} className="mt-0.5 accent-primary outline-none" />
        <span className="flex flex-col gap-0.5"><span className="font-medium">{option.title}</span><span className="text-muted-foreground">{option.description}</span></span>
      </label>)}
    </fieldset>
    <a className="self-start text-sm underline underline-offset-4" href="https://github.com/0xV1p3r/0xSk1ll/blob/main/docs/fleet.md#deploy-the-server" target="_blank" rel="noreferrer"
      onClick={event => { if (isTauri()) { event.preventDefault(); void docs.run(api.openFleetDeploymentDocs); } }}>Deploy the Server</a>
    <ErrorNotice error={docs.error} />
    <div><Button onClick={() => setStep('harnesses')}>Continue</Button></div>
  </Frame>;
  if (step === 'harnesses') return <Frame wide step="Step 2 of 3" title="Choose Harnesses" description="Sync places Skills and Rules into each Harness you keep on. Harnesses are saved in this Machine's machine.toml.">
    <HarnessStep toggle={harnessSave} onFound={setFound} declined={declined} setDeclined={setDeclined} />
    <div className="flex gap-2"><Button variant="outline" disabled={harnessSave.pending} onClick={() => setStep('path')}><ArrowLeft data-icon="inline-start" />Back</Button><Button disabled={harnessSave.pending} onClick={continueToDetails}>Continue</Button></div>
  </Frame>;
  return <DetailsStep path={path} status={status} back={() => setStep('harnesses')} onDone={onDone} />;
}

const loadSetupCandidates = () => api.harnessCandidates(true);

function HarnessStep({ toggle, onFound, declined, setDeclined }: {
  toggle: ReturnType<typeof useMutation>; onFound: (found: HarnessCandidate[]) => void; declined: string[]; setDeclined: (update: (declined: string[]) => string[]) => void;
}) {
  const choices = useResource(api.setupHarnesses, 0);
  const candidates = useResource(loadSetupCandidates, 0);
  const [changing, setChanging] = useState<string>();
  useEffect(() => { onFound(candidates.data ?? []); }, [candidates.data, onFound]);
  if (choices.loading && !choices.data) return <Loading rows={2} />;
  const found = candidates.data ?? [];
  const shown = choices.data?.filter(choice => choice.installed || choice.enabled) ?? [];
  const missing = choices.data?.filter(choice => !choice.installed && !choice.enabled) ?? [];
  const set = (choice: HarnessChoice, enabled: boolean) => { setChanging(choice.name); void toggle.run(() => api.setSetupHarness(choice.name, enabled), next => choices.setData(next)).finally(() => setChanging(undefined)); };
  return <div className="flex flex-col gap-3">
    <ErrorNotice error={choices.error || candidates.error || toggle.error} />
    {choices.data && !shown.length && !found.length && <Notice title="No Harnesses found">Install an agent tool such as Claude Code or Codex, or add a Harness to machine.toml. You can continue now and sync later.</Notice>}
    {shown.length > 0 && <ul className="flex flex-col divide-y rounded-lg border" aria-label="Harnesses">{shown.map(choice => <li key={choice.name}>
      <label className="flex cursor-pointer items-start gap-3 p-3">
        <Checkbox className="mt-0.5" checked={choice.enabled} disabled={toggle.pending || (!!choice.setup_warning && !choice.enabled)} aria-label={`Sync ${choice.name}`} onCheckedChange={enabled => set(choice, enabled)} />
        <span className="flex min-w-0 flex-col gap-0.5"><span className="flex items-center gap-2 font-medium">{choice.name}{changing === choice.name && <LoaderCircle aria-hidden className="size-3.5 animate-spin" />}</span>
          <span className="font-mono text-xs break-all text-muted-foreground">{[choice.harness.skills, choice.harness.instructions].filter(Boolean).join(' · ') || 'Nothing to sync'}</span>{choice.setup_warning && <span className="text-sm text-muted-foreground">{choice.setup_warning}</span>}</span>
      </label></li>)}</ul>}
    {found.length > 0 && <div className="flex flex-col gap-1.5"><p className="text-sm font-medium">Other config folders, set in this app's environment</p>
      <ul className="flex flex-col divide-y rounded-lg border" aria-label="Config folders found in the environment">{found.map(c => <li key={c.name}>
        <label className="flex cursor-pointer items-start gap-3 p-3">
          <Checkbox className="mt-0.5" checked={!c.warning && !declined.includes(c.name)} disabled={!!c.warning || toggle.pending} aria-label={`Sync ${c.name}`}
            onCheckedChange={on => setDeclined(names => on ? names.filter(name => name !== c.name) : [...names, c.name])} />
          <span className="flex min-w-0 flex-col gap-0.5"><span className="font-medium">{c.name}</span>
            <span className="text-xs text-muted-foreground">{c.harness.kind} in <code className="font-mono break-all">{c.harness.home}</code>, from {c.var}</span>
            <span className="font-mono text-xs break-all text-muted-foreground">{[c.harness.skills, c.harness.instructions].filter(Boolean).join(' · ')}</span>
            {c.warning && <span className="text-sm text-muted-foreground">Not offered: it {c.warning}.</span>}</span>
        </label></li>)}</ul></div>}
    {shown.length > 0 && shown.every(choice => !choice.enabled) && <p className="text-muted-foreground" role="status">Nothing is synced until you keep at least one Harness on.</p>}
    {missing.length > 0 && <p className="text-xs text-muted-foreground">Not found on this Machine: {missing.map(choice => choice.name).join(', ')}</p>}
  </div>;
}

function DetailsStep({ path, status, back, onDone }: { path: SetupPath; status: StatusView; back: () => void; onDone: (result: SetupResult) => void }) {
  const run = useMutation();
  const [server, setServer] = useState('');
  const [secret, setSecret] = useState('');
  const [showSecret, setShowSecret] = useState(false);
  const [id, setId] = useState('');
  const [copyFrom, setCopyFrom] = useState('');
  const backButton = <Button type="button" variant="outline" disabled={run.pending} onClick={back}><ArrowLeft data-icon="inline-start" />Back</Button>;
  if (path === 'local') return <Frame wide step="Step 3 of 3" title="Create the skills repo" description="An empty git repo for this Machine's Skills and Rules. Commits stay on this Machine until you add an origin or join a Fleet.">
    <Details rows={[['Repository', <code className="font-mono text-xs break-all">{status.repo}</code>]]} />
    <ErrorNotice error={run.error} />
    <div className="flex gap-2">{backButton}<Button disabled={run.pending} onClick={() => void run.run(api.init, () => onDone({ joined: false }))}><Pending pending={run.pending}>Create skills repo</Pending></Button></div>
  </Frame>;
  const create = path === 'create';
  const secretLabel = create ? 'Admin token' : 'Join code';
  return <Frame wide step="Step 3 of 3" title={create ? 'Join as the first Machine' : 'Join a Fleet'} description={create
    ? 'Registers this Machine as the first one on the Server. The admin token is used once to create a join code and is not saved.'
    : 'Registers this Machine with the Server and clones the Fleet\'s repo. The join code works once.'}>
    <form onSubmit={e => { e.preventDefault(); void run.run(() => api.join({
      server_url: server.trim(), code: create ? '' : secret.trim(), admin_token: create ? secret.trim() : undefined,
      machine_id: id.trim() || undefined, copy_from: copyFrom.trim() || undefined,
    }), () => onDone({ joined: true, copied: !!copyFrom.trim() })); }}><FieldGroup>
      <Field><FieldLabel htmlFor="setup-server">Server URL</FieldLabel><Input id="setup-server" type="url" required value={server} disabled={run.pending}
        onChange={e => setServer(e.target.value)} placeholder="https://skills.example.com" aria-describedby={insecureHttp(server) ? 'setup-http' : undefined} />
        {insecureHttp(server) && <FieldDescription id="setup-http">Plain HTTP sends this Machine's token unencrypted. Use https:// unless the network is trusted.</FieldDescription>}</Field>
      <Field><FieldLabel htmlFor="setup-secret">{secretLabel}</FieldLabel><div className="flex items-center gap-2"><Input id="setup-secret" autoComplete="off" required
        type={showSecret ? 'text' : 'password'} value={secret} disabled={run.pending} onChange={e => setSecret(e.target.value)} />
        <Button type="button" variant="outline" aria-label={`${showSecret ? 'Hide' : 'Show'} ${secretLabel.toLowerCase()}`} aria-pressed={showSecret} onClick={() => setShowSecret(value => !value)}>{showSecret ? 'Hide' : 'Show'}</Button></div>
        {create && <FieldDescription>The value of SK1LL_SERVER_ADMIN_TOKEN on the Server.</FieldDescription>}</Field>
      <Field><FieldLabel htmlFor="setup-machine-id">Machine id</FieldLabel><Input id="setup-machine-id" value={id} placeholder={status.machine_id} disabled={run.pending} onChange={e => setId(e.target.value)} aria-describedby="setup-machine-id-hint" />
        <FieldDescription id="setup-machine-id-hint">Optional. Defaults to {status.machine_id}.</FieldDescription></Field>
      {!create && <Field><FieldLabel htmlFor="setup-copy-from">Copy Assignment from Machine</FieldLabel><Input id="setup-copy-from" value={copyFrom} placeholder="Machine id" disabled={run.pending} onChange={e => setCopyFrom(e.target.value)} aria-describedby="setup-copy-from-hint" />
        <FieldDescription id="setup-copy-from-hint">Optional. Otherwise this Machine starts with everything off, and the next step offers to turn it on.</FieldDescription></Field>}
      <FieldError>{run.error}</FieldError>
      <div className="flex gap-2">{backButton}<Button type="submit" disabled={run.pending}><Pending pending={run.pending}>Join Fleet</Pending></Button></div>
    </FieldGroup></form>
  </Frame>;
}

// After joining without --copy-from, the Server gives this Machine an all-off Assignment.
export function StartingAssignment({ status, onDone, openMatrix }: { status: StatusView; onDone: () => void; openMatrix: () => void }) {
  const run = useMutation();
  // What turning everything on could not do, shown before leaving this screen. A retry that fails before writing keeps it.
  const [left, setLeft] = useState<{ errors: string[]; empty: boolean }>();
  const connected = status.connection === 'connected';
  return <Frame wide title="Choose the starting Assignment" description={`${status.machine_id} joined the Fleet. Its Harnesses start with every Skill and Rule off.`}>
    {!connected && <Badge variant="secondary" role="status"><LoaderCircle className="animate-spin" />Connecting to the Server…</Badge>}
    <div className="flex flex-col gap-2">
      <Button className="self-start" disabled={!connected || run.pending} onClick={() => void run.run(async () => {
        // The Daemon sets every Baseline item and both defaults per Harness, including items this Machine hasn't pulled yet.
        const errors: string[] = [], harnesses = await api.harnesses();
        for (const { name } of harnesses) {
          try {
            const result = await api.setHarnessAssignment(name, true);
            errors.push(...(result.errors ?? []).map(error => `${name}: ${error}`));
          } catch (e) { errors.push(`${name}: ${errorMessage(e)}`); }
        }
        return { errors, empty: !harnesses.length };
      }, value => { if (value.errors.length || value.empty) setLeft(value); else onDone(); })}><Pending pending={run.pending}>Turn everything on</Pending></Button>
      <p className="text-xs text-muted-foreground">Every Skill and Rule in every Harness on this Machine, including ones added later.</p>
    </div>
    <ErrorNotice error={run.error} />
    {left?.empty && <Notice title="No Harness is on">There was nothing to turn on. Turn Harnesses on in Settings, then set the Assignment in the matrix.</Notice>}
    {!!left?.errors.length && <Notice title="Not everything was turned on" tone="destructive">
      <ul className="list-disc pl-5">{left.errors.map(line => <li key={line} className="break-words">{line}</li>)}</ul></Notice>}
    <div className="flex flex-wrap gap-2">{left && <Button onClick={onDone}>Continue</Button>}<Button variant="outline" disabled={run.pending} onClick={openMatrix}>Choose in the Assignment matrix</Button>
      {!left && <Button variant="ghost" disabled={run.pending} onClick={onDone}>Leave everything off</Button>}</div>
  </Frame>;
}

const gettingStartedKey = 'sk1ll-getting-started';
export const showGettingStarted = () => { try { localStorage.setItem(gettingStartedKey, 'show'); } catch { /* Storage may be unavailable. */ } };
export const gettingStartedShown = () => { try { return localStorage.getItem(gettingStartedKey) === 'show'; } catch { return false; } };
const hideGettingStarted = () => { try { localStorage.removeItem(gettingStartedKey); } catch { /* Storage may be unavailable. */ } };

// Shown in the Library after the setup wizard until every step is done or it is dismissed.
export function GettingStarted({ status, entries, machines, scope, addImport, newSkill, newRule, showUnmanaged, reviewDraft, reopened, onDismiss }: {
  status: StatusView; entries: ContentEntry[] | undefined; machines: number | undefined; scope?: Scope;
  reopened: boolean; onDismiss: () => void; addImport: () => void; newSkill: () => void; newRule: () => void; showUnmanaged: () => void; reviewDraft: () => void;
}) {
  const [visible, setVisible] = useState(() => reopened || gettingStartedShown());
  const standalone = status.mode === 'standalone';
  const unmanaged = entries?.filter(entry => entry.type === 'unmanaged').length ?? 0;
  const added = !!entries?.some(entry => entry.type !== 'unmanaged');
  const { unpushed, blocked, clashes } = status.report;
  const committed = status.draft_commit_complete;
  const waiting = standalone || committed ? '' : blocked || clashes.length ? 'Blocked by the new Baseline' : unpushed.length ? `${plural(unpushed.length, 'item')} not pushed` : '';
  // Optional steps never keep the checklist open: some Unmanaged skills are meant to stay that way.
  const steps: { title: string; done: boolean; optional?: boolean; note?: string; action?: ReactNode }[] = [
    { title: 'Add a Skill or Rule', done: added,
      action: <><Button size="sm" onClick={addImport}>Add Import</Button><Button size="sm" variant="outline" onClick={newSkill}>New Skill</Button><Button size="sm" variant="outline" onClick={newRule}>New Rule</Button></> },
    ...(unmanaged ? [{ title: `Adopt ${plural(unmanaged, 'Unmanaged skill')} already in your Harnesses`, done: false, optional: true,
      action: <Button size="sm" variant="outline" onClick={showUnmanaged}>Show</Button> }] : []),
    { title: standalone ? 'Commit your Draft' : 'Commit your Draft to the Fleet', done: committed, note: waiting || undefined,
      action: <Button size="sm" variant="outline" onClick={reviewDraft}>Review Draft</Button> },
    ...(standalone ? [] : [{ title: 'Add another Machine', done: (machines ?? 0) > 1, action: <JoinCodeButton label="Create join code" serverUrl={status.server_url} scope={scope} /> }]),
  ];
  const known = !!entries && (standalone || machines !== undefined);
  const required = steps.filter(step => !step.optional);
  const finished = known && required.every(step => step.done);
  // Reopening a finished checklist keeps it open; one with steps left still closes when they're done.
  const [sawUnfinished, setSawUnfinished] = useState(false);
  useEffect(() => { if (known && !finished) setSawUnfinished(true); }, [known, finished]);
  const keepOpen = reopened && !sawUnfinished;
  useEffect(() => { if (visible && finished && !keepOpen) { hideGettingStarted(); setVisible(false); onDismiss(); } }, [visible, finished, keepOpen, onDismiss]);
  if (!visible || (finished && !keepOpen)) return null;
  return <Panel label="Getting started"><PanelToolbar><SectionTitle>Getting started</SectionTitle>
    <span className="text-xs text-muted-foreground tabular-nums">{required.filter(step => step.done).length} of {required.length} done</span>
    <Button variant="ghost" size="sm" className="ml-auto" onClick={() => { hideGettingStarted(); setVisible(false); onDismiss(); }}>Dismiss</Button></PanelToolbar>
    <ol className="divide-y">{steps.map(step => <li key={step.title} className="flex flex-wrap items-center gap-3 px-3.5 py-2.5">
      <span aria-hidden className={cn('grid size-5 shrink-0 place-items-center rounded-full border', step.done && 'border-success bg-success text-background')}>{step.done && <Check className="size-3" />}</span>
      <span className={cn('min-w-0 flex-1 text-sm', step.done && 'text-muted-foreground line-through')}>{step.title}{step.optional && <Badge variant="secondary" className="ml-2">Optional</Badge>}{step.note && <span className="block text-xs text-muted-foreground">{step.note}</span>}<span className="sr-only">{step.done ? ', done' : step.optional ? ', optional' : ', to do'}</span></span>
      {!step.done && <div className="flex gap-2">{step.action}</div>}
    </li>)}</ol>
  </Panel>;
}
