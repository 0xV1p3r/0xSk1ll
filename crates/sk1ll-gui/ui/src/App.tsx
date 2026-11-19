import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { ArrowDownToLine, BookOpen, FilePen, History, LayoutGrid, LoaderCircle, RefreshCw, Search, Server, Settings as SettingsIcon, TriangleAlert } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Separator } from '@/components/ui/separator';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupContent, SidebarGroupLabel, SidebarHeader, SidebarInset,
  SidebarMenu, SidebarMenuBadge, SidebarMenuButton, SidebarMenuItem, SidebarProvider, SidebarRail, SidebarTrigger, useSidebar,
} from '@/components/ui/sidebar';
import { ThemeSelector } from '@/components/theme-selector';
import symbol from '@branding/symbol.svg';
import { cn } from '@/lib/utils';
import { api, errorMessage } from './api';
import { toast, ToastProvider } from './toast';
import { CommandPalette } from './CommandPalette';
import { Fleet } from './Fleet';
import { Matrix } from './Matrix';
import { Draft } from './Draft';
import { Conflicts } from './Conflicts';
import { Imports } from './Imports';
import { Log } from './Log';
import { Skills, type SkillsHandoff } from './Skills';
import { canLeaveFile, type FileEditState } from './SkillFile';
import { useResource } from './hooks';
import { eventTopics } from './invalidation';
import { plural } from './model';
import { Settings, ownScope } from './Settings';
import { ConnectionPage, GettingStarted, RestartingPage, SetupWizard, StartingAssignment, Wordmark, showGettingStarted, type SetupResult } from './Setup';
import { ErrorNotice, Loading, Notice } from './shared';
import type { CommandError, DaemonConnection, StatusView } from './types';

const screens = {
  Fleet: { icon: Server, group: 'Fleet', description: 'Machines sharing this Baseline. Reports refresh live and every 15 seconds.' },
  'Assignment matrix': { icon: LayoutGrid, group: 'Fleet', description: 'Assignment takes effect immediately. Explicit toggles override a Harness default.' },
  'Assignment log': { icon: History, group: 'Fleet', description: 'Assignment log, newest first. Restore applies an entry\'s previous value and adds a new log entry.' },
  Library: { icon: BookOpen, group: 'This Machine', description: 'Browse Personal Skills, Imports, Rules, and Unmanaged skills on this Machine.' },
  Draft: { icon: FilePen, group: 'This Machine', description: 'This Machine\'s Draft is live in its Harnesses. Commit sends selected items to the Fleet Baseline.' },
  Conflicts: { icon: TriangleAlert, group: 'This Machine', description: 'A Conflict is a Harness edit that differs from this Machine\'s Draft.' },
  Imports: { icon: ArrowDownToLine, group: 'This Machine', description: 'Available upstream updates. Review each update in the Draft before Commit.' },
  Settings: { icon: SettingsIcon, group: 'Settings', description: 'Harnesses, Fleet membership, and the Daemon connection for this Machine.' },
} as const;
type Screen = keyof typeof screens;
const screenHash = (screen: Screen) => `#/${screen.toLowerCase().replaceAll(' ', '-')}`;
export const screenFromHash = (hash: string): Screen => (Object.keys(screens) as Screen[]).find(screen => screenHash(screen) === hash) ?? 'Fleet';
const visibleScreens = (status: StatusView) => (Object.keys(screens) as Screen[]).filter(name => status.mode !== 'standalone' || (name !== 'Fleet' && name !== 'Assignment log'));
const screenLabel = (name: Screen, status: StatusView) => status.mode === 'standalone' && name === 'Assignment matrix' ? 'Assignment' : name;
const paletteShortcut = /Mac|iPhone|iPad|iPod/i.test(navigator.platform + navigator.userAgent) ? '⌘K' : 'Ctrl K';
const connectionLabel = (connection: StatusView['connection']) => { const text = connection.replaceAll('_', ' '); return text.charAt(0).toUpperCase() + text.slice(1); };
const sidebarKey = 'sk1ll-sidebar-open';
function storedSidebar() {
  try { const value = localStorage.getItem(sidebarKey); return value === 'true' ? true : value === 'false' ? false : null; } catch { return null; }
}
export function AppSidebar({ screen, setScreen, status, unmanagedCount, recoveredCount = 0, importUpdates = 0, importErrors = 0 }: { screen: Screen; setScreen: (screen: Screen) => void; status: StatusView; unmanagedCount: number; recoveredCount?: number; importUpdates?: number; importErrors?: number }) {
  const { isMobile, setOpenMobile } = useSidebar();
  const counts: Partial<Record<Screen, number>> = { Library: unmanagedCount, Draft: status.report.drafts.length, Conflicts: status.report.conflicts.length + recoveredCount, Imports: importUpdates + importErrors };
  const item = (name: Screen) => {
    const Icon = screens[name].icon;
    return <SidebarMenuItem key={name}>
      <SidebarMenuButton isActive={screen === name} tooltip={screenLabel(name, status)} aria-current={screen === name ? 'page' : undefined}
        onClick={() => { if (isMobile) setOpenMobile(false); setScreen(name); }}><Icon /><span>{screenLabel(name, status)}</span></SidebarMenuButton>
      {!!counts[name] && <span aria-hidden className={cn('pointer-events-none absolute top-1 right-1 hidden size-2 rounded-full group-data-[collapsible=icon]:block', name === 'Conflicts' || (name === 'Imports' && importErrors > 0) ? 'bg-warning' : 'bg-brand-ink')} />}
      {!!counts[name] && <SidebarMenuBadge className={cn(name === 'Conflicts' || (name === 'Imports' && importErrors > 0) ? 'bg-warning/13 text-warning-foreground' : 'bg-brand-soft text-brand-ink')} aria-label={name === 'Library' ? `${counts[name]} Unmanaged skill${counts[name] === 1 ? '' : 's'} to adopt` : name === 'Conflicts' ? [status.report.conflicts.length && `${plural(status.report.conflicts.length, 'Conflict')} to resolve`, recoveredCount && `${plural(recoveredCount, 'recovered item')} to review`].filter(Boolean).join(', ') : name === 'Imports' ? [importUpdates && plural(importUpdates, 'Import update'), importErrors && plural(importErrors, 'failed check')].filter(Boolean).join(', ') : plural(counts[name] ?? 0, 'Draft item')}>{counts[name]}</SidebarMenuBadge>}
    </SidebarMenuItem>;
  };
  const group = (label: string) => visibleScreens(status).filter(name => (status.mode === 'standalone' && name === 'Assignment matrix' ? 'This Machine' : screens[name].group) === label);
  return <Sidebar collapsible="icon">
    <SidebarHeader><div className="flex h-12 items-center gap-2 overflow-hidden rounded-md p-2 group-data-[collapsible=icon]:size-8! group-data-[collapsible=icon]:p-0!">
      <img src={symbol} alt="0xSk1ll" className="size-8 shrink-0 object-contain" />
      <div className="grid min-w-0 flex-1 gap-1 text-left text-sm leading-tight group-data-[collapsible=icon]:hidden"><span aria-hidden className="truncate font-semibold">0xSk1ll</span>
        <span className="truncate font-mono text-[11px] text-muted-foreground">{status.machine_id}</span></div>
    </div></SidebarHeader>
    <SidebarContent><nav aria-label="Main navigation" className="contents">
      {(status.mode === 'standalone' ? ['This Machine'] : ['Fleet', 'This Machine']).map(label => <SidebarGroup key={label}><SidebarGroupLabel className="group-data-[collapsible=icon]:pointer-events-none">{label}</SidebarGroupLabel>
        <SidebarGroupContent><SidebarMenu>{group(label).map(item)}</SidebarMenu></SidebarGroupContent></SidebarGroup>)}
      <SidebarGroup className="mt-auto"><SidebarGroupContent><SidebarMenu>{item('Settings')}</SidebarMenu></SidebarGroupContent></SidebarGroup>
    </nav></SidebarContent>
    <SidebarFooter />
    <SidebarRail />
  </Sidebar>;
}

export function App() {
  const [requestedScreen, setScreen] = useState<Screen>(() => screenFromHash(window.location.hash));
  const [paletteOpen, setPaletteOpen] = useState(false);
  const sidebarPreference = useRef(storedSidebar());
  const [sidebarOpen, setSidebarOpen] = useState(() => sidebarPreference.current ?? window.innerWidth >= 1000);
  const changeSidebar = useCallback((open: boolean) => {
    sidebarPreference.current = open; setSidebarOpen(open);
    try { localStorage.setItem(sidebarKey, String(open)); } catch { return; }
  }, []);
  const [skillsHandoff, setSkillsHandoff] = useState<SkillsHandoff>({});
  // Each request increments, so the dialog opens again even when Imports is already shown.
  const [openAdd, setOpenAdd] = useState(0);
  const [matrixTarget, setMatrixTarget] = useState<{ item: string; harness: string }>();
  const [status, setStatus] = useState<StatusView>();
  const [checklist, setChecklist] = useState(0);
  const [checklistReopened, setChecklistReopened] = useState(false);
  const [fleetTarget, setFleetTarget] = useState<{ id: string; visit: number }>();
  const [apiMismatch, setApiMismatch] = useState('');
  const closeChecklist = useCallback(() => setChecklistReopened(false), []);
  const [reachable, setReachable] = useState<boolean>();
  const [error, setError] = useState('');
  const [socket, setSocket] = useState('Default Daemon socket');
  const [revisions, setRevisions] = useState({ status: 0, content: 0, imports: 0, fleet: 0, history: 0 });
  const invalidate = useCallback((topics: (keyof typeof revisions)[]) => setRevisions(value => {
    const next = { ...value }; topics.forEach(topic => next[topic]++); return next;
  }), []);
  const changed = useCallback(() => invalidate(['status', 'content', 'imports', 'fleet', 'history']), [invalidate]);
  const revision = revisions.content;
  const standalone = status?.mode === 'standalone';
  const screen = standalone && (requestedScreen === 'Fleet' || requestedScreen === 'Assignment log') ? 'Library' : requestedScreen;
  const ready = !!status && status.repo_ready !== false;
  // Set when setup joins a Fleet: the Daemon restarts, then the Machine needs a starting Assignment.
  const [joinedAt, setJoinedAt] = useState<number>();
  const [startingAssignment, setStartingAssignment] = useState(false);
  const setupDone = (result: SetupResult) => {
    showGettingStarted();
    if (result.joined) { setJoinedAt(Date.now()); setStartingAssignment(!result.copied); }
    else void api.status().then(setStatus).catch(() => {});
    changed();
  };
  useEffect(() => {
    if (joinedAt === undefined) return;
    if (status?.mode === 'joined') { setJoinedAt(undefined); return; }
    // Give up waiting so a Daemon that fails to restart gets the start screen.
    const timer = setTimeout(() => setJoinedAt(undefined), 30000);
    return () => clearTimeout(timer);
  }, [joinedAt, status?.mode]);
  const skills = useResource(api.skills, revision, undefined, ready);
  const recovered = useResource(api.recovered, revision, undefined, ready);
  const imports = useResource(api.imports, revisions.imports, undefined, ready);
  const fleet = useResource(api.fleet, revisions.fleet, undefined, !!status && !standalone);
  const [refreshPending, setRefreshPending] = useState(false);
  const refreshLock = useRef(false);
  const refreshHeader = async () => {
    if (refreshLock.current) return;
    refreshLock.current = true; setRefreshPending(true); changed();
    try { const [next] = await Promise.all([api.status(), new Promise(resolve => setTimeout(resolve, 350))]); setStatus(next); setReachable(true); setApiMismatch(''); setError(''); }
    catch (e) { setError(errorMessage(e)); }
    finally { refreshLock.current = false; setRefreshPending(false); }
  };
  const runAction = async (action: () => Promise<{ title: string; description?: string; tone?: 'destructive' }>) => {
    try { toast(await action()); changed(); } catch (e) { toast({ title: 'Action failed', description: errorMessage(e), tone: 'destructive' }); }
  };
  const heading = useRef<HTMLHeadingElement>(null);
  const navigated = useRef(false);
  const fileEditState = useRef<FileEditState>('clean');
  const navigate = useCallback((next: Screen, machine?: string) => { if (!canLeaveFile(fileEditState.current)) return false; navigated.current = true; setMatrixTarget(undefined); setFleetTarget(previous => machine ? { id: machine, visit: (previous?.visit ?? 0) + 1 } : undefined); setOpenAdd(0); setSkillsHandoff({}); setScreen(next); window.location.hash = screenHash(next); return true; }, []);
  useEffect(() => {
    const hashchange = () => { const next = screenFromHash(window.location.hash); if (next !== screen && !navigate(next)) window.history.replaceState(null, '', screenHash(screen)); };
    window.addEventListener('hashchange', hashchange);
    return () => window.removeEventListener('hashchange', hashchange);
  }, [navigate, screen]);
  useEffect(() => {
    let wide = window.innerWidth >= 1000;
    const resize = () => {
      const next = window.innerWidth >= 1000;
      if (next !== wide) setSidebarOpen(next && (sidebarPreference.current ?? true));
      wide = next;
    };
    const keydown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); setPaletteOpen(value => !value); }
      if (event.key === '/' && !event.metaKey && !event.ctrlKey && !event.altKey && !(event.target instanceof HTMLElement && event.target.closest('input, textarea, select, [contenteditable="true"]')) && !document.querySelector('[role="dialog"]')) {
        const input = document.querySelector<HTMLInputElement>('#main input[type="search"], #main input[aria-label*="Search"], #main input[placeholder*="Search"]');
        if (input) { event.preventDefault(); input.focus(); }
      }
    };
    window.addEventListener('resize', resize); window.addEventListener('keydown', keydown);
    return () => { window.removeEventListener('resize', resize); window.removeEventListener('keydown', keydown); };
  }, []);
  useEffect(() => {
    // Move keyboard and screen-reader focus to the new screen after navigation.
    if (navigated.current) { navigated.current = false; heading.current?.focus(); }
  }, [screen]);
  useEffect(() => {
    let alive = true;
    let refreshing = false;
    let refreshAgain = false;
    let eventTimer: ReturnType<typeof setTimeout> | undefined;
    const refresh = async () => {
      if (refreshing) { refreshAgain = true; return; }
      refreshing = true;
      try {
        const next = await api.status();
        if (alive) { setStatus(next); setReachable(true); setApiMismatch(''); setError(''); }
      } catch (e) {
        if (alive) { const failure = e as CommandError; if (failure?.status === 426) setApiMismatch(errorMessage(e)); if (failure?.unreachable) { setReachable(false); setApiMismatch(''); } setError(errorMessage(e)); }
      } finally {
        refreshing = false;
        if (alive && refreshAgain) { refreshAgain = false; void refresh(); }
      }
    };
    const pending = new Set<keyof typeof revisions>();
    const hint = (topics: (keyof typeof revisions)[]) => {
      topics.forEach(topic => pending.add(topic));
      if (eventTimer) clearTimeout(eventTimer);
      eventTimer = setTimeout(() => { invalidate([...pending]); pending.clear(); }, 120);
    };
    void api.connectionInfo().then(value => { if (alive) setSocket(value); }).catch(e => { if (alive) setError(errorMessage(e)); });
    void refresh();
    const pollStatus = setInterval(() => void refresh(), 5000);
    // History too: an open Assignment log has no other fallback for a missed fleet event.
    const pollFleet = setInterval(() => invalidate(['fleet', 'history']), 15000);
    const unlisteners: (() => void)[] = [];
    const subscriptions = [
      ...['status', 'draft', 'assignment', 'imports', 'fleet'].map(name => listen(name, () => hint(eventTopics(name)))),
      listen<DaemonConnection>('daemon-connection', event => {
        if (!alive) return;
        if (!event.payload.reachable) setError(event.payload.message ?? 'Daemon is unreachable');
        hint(['status', 'content', 'imports', 'fleet', 'history']);
      }),
    ];
    for (const subscription of subscriptions) {
      void subscription.then(unlisten => { if (alive) unlisteners.push(unlisten); else unlisten(); })
        .catch(e => { if (alive) setError(errorMessage(e)); });
    }
    // Covers initial events emitted before React registered its listeners.
    void refresh();
    return () => {
      alive = false; clearInterval(pollStatus); clearInterval(pollFleet);
      if (eventTimer) clearTimeout(eventTimer);
      unlisteners.forEach(unlisten => unlisten());
    };
  }, [invalidate]);
  useEffect(() => {
    // Local changes and explicit refreshes invalidate the machine header.
    let alive = true;
    void api.status().then(value => { if (alive) { setStatus(value); setReachable(true); } }).catch(() => {});
    return () => { alive = false; };
  }, [revisions.status]);
  if (apiMismatch) return <TooltipProvider><ConnectionPage socket={socket} error={apiMismatch} apiMismatch /></TooltipProvider>;
  // Give the restart after a join time before offering to start the Daemon.
  if (joinedAt !== undefined && status?.mode !== 'joined') return <TooltipProvider><RestartingPage /></TooltipProvider>;
  if (reachable === false) return <TooltipProvider><ConnectionPage socket={socket} error={error} /></TooltipProvider>;
  if (!status) return <main className="grid min-h-svh place-items-center bg-muted/40 p-6"><div className="flex w-full max-w-sm flex-col items-start gap-3">
    <Wordmark /><p className="flex items-center gap-2 text-sm text-muted-foreground" role="status"><LoaderCircle aria-hidden className="size-4 animate-spin" />Connecting to the Daemon…</p><ErrorNotice error={error} /></div></main>;
  if (standalone && !status.repo_ready) return <TooltipProvider><SetupWizard status={status} onDone={setupDone} /></TooltipProvider>;
  if (startingAssignment && status.mode === 'joined') return <TooltipProvider><StartingAssignment status={status} onDone={() => { setStartingAssignment(false); changed(); }}
    openMatrix={() => { setStartingAssignment(false); navigate('Assignment matrix'); }} /></TooltipProvider>;
  const serverConnected = !standalone && status.connection === 'connected';
  const conflict = status.report.conflicts[0];
  const server = status.server_url?.replace(/^https?:\/\//, '').replace(/\/$/, '') || 'Server';
  return <ToastProvider><TooltipProvider><SidebarProvider open={sidebarOpen} onOpenChange={changeSidebar}>
    <a href="#main" onClick={event => { event.preventDefault(); document.getElementById('main')?.focus(); }} className="sr-only z-50 rounded-md bg-background px-3 py-2 text-sm shadow focus:not-sr-only focus:fixed focus:top-2 focus:left-2">Skip to content</a>
    <AppSidebar screen={screen} setScreen={navigate} status={status} recoveredCount={recovered.data?.length ?? 0} importUpdates={imports.data?.updates.length ?? 0} importErrors={imports.data?.errors.length ?? 0} unmanagedCount={skills.data?.filter(entry => entry.type === 'unmanaged').length ?? 0} />
    <SidebarInset className="min-w-0">
      <header className="sticky top-0 z-10 flex h-12 shrink-0 items-center gap-2 border-b bg-background px-4 md:gap-3 md:px-[18px]">
        <SidebarTrigger className="-ml-1 size-8" />
        <Separator orientation="vertical" className="data-vertical:h-4 data-vertical:self-auto" />
        <span className="hidden min-w-0 truncate text-xs text-muted-foreground md:block">{status.machine_id}{!standalone && ` → ${server}`}</span>
        <Button variant="search" className="ml-auto h-8 min-w-0 gap-2 lg:w-full lg:max-w-72" aria-label="Open command palette" onClick={() => setPaletteOpen(true)}>
          <Search data-icon="inline-start" /><span className="hidden truncate lg:inline">Jump to machine, skill, action…</span><kbd className="ml-auto rounded border px-1 font-mono text-[10px]">{paletteShortcut}</kbd>
        </Button>
        <span className="flex shrink-0 items-center gap-2 text-xs text-muted-foreground" role="status" aria-label={`${standalone ? '' : 'Server '}${connectionLabel(standalone ? 'standalone' : status.connection)}`}>
          <span className={cn('size-2 shrink-0 rounded-full', standalone || serverConnected ? 'bg-success' : 'bg-warning')} aria-hidden />
          <span className="hidden sm:inline">{serverConnected ? 'Connected' : connectionLabel(standalone ? 'standalone' : status.connection)}{serverConnected && status.assignment_revision != null && ` · rev ${status.assignment_revision}`}</span>
        </span>
        <Tooltip><TooltipTrigger render={<Button variant="ghost" size="icon" aria-label="Refresh" aria-busy={refreshPending} onClick={() => void refreshHeader()} />}>
          <RefreshCw className={cn(refreshPending && 'animate-spin')} /></TooltipTrigger><TooltipContent>Refresh</TooltipContent></Tooltip>
        <ThemeSelector />
      </header>
      <main data-library-search={skillsHandoff.search} id="main" tabIndex={-1} className="mx-auto flex w-full max-w-6xl min-w-0 flex-col gap-5 p-4 md:px-[18px] md:py-4">
        <div className="flex flex-col gap-1"><div className="flex items-center gap-2"><h1 ref={heading} tabIndex={-1} className="rounded-sm text-2xl font-semibold tracking-tight outline-none focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-ring">{screenLabel(screen, status)}</h1></div>
          <p className="text-sm text-muted-foreground">{screen === 'Draft' && status.revoked ? "This Machine's Draft is private. The Daemon has stopped managing its Harnesses until this Machine rejoins." : screen === 'Draft' && status.awaiting_assignment ? "This Machine's Draft is private. Harnesses stay unchanged until the Server sends an Assignment." : standalone && screen === 'Draft' ? 'Commit saves selected Draft items to the local repository.' : standalone && screen === 'Settings' ? 'Harnesses, local repository, and Fleet membership for this Machine.' : screens[screen].description}</p></div>
        {status.revoked && <Notice title="Machine revoked" tone="destructive">This Machine was removed from the Fleet. The Daemon has stopped managing its Harnesses.
          <div><Button variant="outline" size="sm" className="mt-2" onClick={() => navigate('Settings')}>Rejoin in Settings</Button></div></Notice>}
        {status.awaiting_assignment && <Notice title="Awaiting Assignment">The Daemon has no Assignment snapshot. Harnesses are left alone until the Server sends one.</Notice>}
        {status.connection === 'auth_rejected' && <Notice title="Server authentication rejected" tone="destructive">The Daemon is using its cached Assignment and keeps reconnecting.
          {status.last_error && <p className="font-mono text-xs">{status.last_error}</p>}
          <p>If this Machine was removed or its credential rotated, create a join code on another Machine and rejoin.</p>
          <div><Button variant="outline" size="sm" className="mt-2" onClick={() => navigate('Settings')}>Rejoin in Settings</Button></div></Notice>}
        <ErrorNotice error={error} />
        {conflict && (screen === 'Library' || screen === 'Draft') && <Alert variant="warning" role="status" className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg px-3 py-2.5">
          <TriangleAlert /><AlertTitle>This Machine has {status.report.conflicts.length === 1 ? 'a Conflict' : `${status.report.conflicts.length} Conflicts`}</AlertTitle>
          <AlertDescription className="min-w-0 [overflow-wrap:anywhere]"><code className="font-mono">{conflict.item.replace(/^(skill|rule):/, '')}</code> was edited in {conflict.harness} after the last sync.</AlertDescription>
          <Button size="sm" className="ml-auto" onClick={() => navigate('Conflicts')}>Resolve</Button>
        </Alert>}
        {screen === 'Library' && <GettingStarted key={checklist} reopened={checklistReopened} onDismiss={closeChecklist} status={status} entries={skills.data} machines={fleet.data?.machines.length} scope={ownScope(fleet.data, status.machine_id)}
          addImport={() => { navigate('Imports'); setOpenAdd(n => n + 1); }} newSkill={() => setSkillsHandoff({ create: 'skill' })}
          newRule={() => setSkillsHandoff({ create: 'rule' })} showUnmanaged={() => setSkillsHandoff({ filter: 'unmanaged' })} reviewDraft={() => navigate('Draft')} />}
        {screen === 'Library' && <><ErrorNotice error={skills.error} />{skills.loading && !skills.data ? <Loading /> : <Skills fileEditState={fileEditState} handoff={skillsHandoff} connection={status.connection} entries={skills.data ?? []} revision={revision} changed={changed} reviewDraft={() => navigate('Draft')} assignment={(item, harness) => { navigate('Assignment matrix'); setMatrixTarget({ item, harness }); }} addImport={() => { navigate('Imports'); setOpenAdd(n => n + 1); }} />}</>}
        {screen === 'Fleet' && <Fleet key={fleetTarget?.visit} initialMachine={fleetTarget?.id} serverUrl={status.server_url} revision={revision + revisions.fleet} machineId={status.machine_id} localReport={status.report} unmanaged={skills.data?.filter(entry => entry.type === 'unmanaged')} discovery={{ loading: !skills.data && skills.loading, error: skills.error }} navigate={next => { navigate(next); if (next === 'Library') setSkillsHandoff({ filter: 'unmanaged' }); }} changed={changed} live={status.connection === 'connected'} />}
        {screen === 'Assignment matrix' && <Matrix revision={revision + revisions.fleet} status={status} target={matrixTarget} openLog={() => navigate('Assignment log')} />}
        {screen === 'Draft' && <Draft revision={revision} status={status} changed={changed} />}
        {screen === 'Conflicts' && <Conflicts revision={revision} changed={changed} reviewDraft={() => navigate('Draft')} />}
        {screen === 'Imports' && <Imports openAdd={openAdd} revision={revisions.imports} changed={changed} reviewDraft={() => navigate('Draft')} />}
        {screen === 'Assignment log' && <Log fleetRevision={revisions.fleet} revision={revision + revisions.history} changed={changed} />}
        {screen === 'Settings' && <Settings fleetRevision={revisions.fleet} revision={revision} status={status} socket={socket} changed={changed} />}
      </main>
    </SidebarInset>
    <CommandPalette open={paletteOpen} onOpenChange={setPaletteOpen} commands={[
      ...visibleScreens(status).map(name => ({ id: `screen-${name.replaceAll(' ', '-')}`, label: `Go to: ${screenLabel(name, status)}`, select: () => navigate(name) })),
      ...(!standalone ? fleet.data?.machines ?? [] : []).map(machine => ({ id: `machine-${machine.id}`, label: `Machine: ${machine.id}`, select: () => navigate('Fleet', machine.id) })),
      ...(!standalone || status.has_origin ? [{ id: 'action-pull', background: true, label: 'Action: Pull Baseline', select: () => void runAction(async () => { const result = await api.pull(); return { title: result.state === 'blocked' ? 'Pull blocked' : result.state === 'up_to_date' ? 'Baseline is up to date' : 'Pull complete' }; }) },
      { id: 'action-push', background: true, label: 'Action: Push', select: () => void runAction(async () => { const result = await api.push(); return { title: result.pushed ? 'Push complete' : standalone ? 'Push did not reach origin' : 'Push did not reach the Server', tone: result.pushed ? undefined : 'destructive' }; }) },
      ] : []),
      { id: 'action-check-imports', background: true, label: 'Action: Check Imports', select: () => void runAction(async () => { const result = await api.checkImports(); return { title: 'Import check complete', description: `${plural(result.updates.length, 'update')}, ${plural(result.errors.length, 'failed check')}`, tone: result.errors.length ? 'destructive' : undefined }; }) },
      { id: 'action-getting-started', label: 'Action: Show getting started', select: () => { if (!navigate('Library')) return; showGettingStarted(); setChecklistReopened(true); setChecklist(value => value + 1); } },
      { id: 'action-add-import', label: 'Action: Add Import', select: () => { navigate('Imports'); setOpenAdd(n => n + 1); } },
      ...(['Skill', 'Rule'] as const).map(type => ({ id: `action-new-${type.toLowerCase()}`, label: `Action: New ${type}`, select: () => { if (!navigate('Library')) return; setSkillsHandoff({ create: type === 'Skill' ? 'skill' : 'rule' }); } })),
      ...(skills.data ?? []).map((entry, index) => ({ id: `content-${index}`, label: `${entry.type === 'rule' ? 'Rule' : 'Skill'}: ${entry.name}`, select: () => { if (!navigate('Library')) return; setSkillsHandoff({ search: entry.name }); } })),
    ]} />
  </SidebarProvider></TooltipProvider></ToastProvider>;
}
