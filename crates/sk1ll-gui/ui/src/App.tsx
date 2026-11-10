import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { ArrowDownToLine, BookOpen, FilePen, History, Info, LayoutGrid, LoaderCircle, RefreshCw, Search, Server, Settings as SettingsIcon, TriangleAlert } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Separator } from '@/components/ui/separator';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupContent, SidebarGroupLabel, SidebarHeader, SidebarInset,
  SidebarMenu, SidebarMenuBadge, SidebarMenuButton, SidebarMenuItem, SidebarProvider, SidebarRail, SidebarTrigger, useSidebar,
} from '@/components/ui/sidebar';
import { ThemeSelector } from '@/components/theme-selector';
import symbol from '@branding/symbol.svg';
import wordmarkDark from '@branding/wordmark-dark.svg';
import wordmarkLight from '@branding/wordmark-light.svg';
import { cn } from '@/lib/utils';
import { api, errorMessage } from './api';
import { ToastProvider } from './toast';
import { CommandPalette } from './CommandPalette';
import { Fleet } from './Fleet';
import { Matrix } from './Matrix';
import { Draft } from './Draft';
import { Conflicts, Imports, Log } from './Activity';
import { Skills } from './Skills';
import { useResource } from './hooks';
import { Settings } from './Settings';
import { ErrorNotice, Loading, Notice } from './shared';
import type { CommandError, DaemonConnection, StatusView } from './types';

const screens = {
  Fleet: { icon: Server, group: 'Fleet', description: 'Machines sharing this Baseline. Reports refresh live and every 15 seconds.' },
  'Assignment matrix': { icon: LayoutGrid, group: 'Fleet', description: 'Assignment takes effect immediately. Explicit toggles override a Harness default.' },
  Log: { icon: History, group: 'Fleet', description: 'Assignment log, newest first. Restore applies an entry\'s previous value and adds a new log entry.' },
  Library: { icon: BookOpen, group: 'This Machine', description: 'Browse Personal Skills, Imports, Rules, and Unmanaged skills on this Machine.' },
  Draft: { icon: FilePen, group: 'This Machine', description: 'This Machine\'s Draft is live in its Harnesses. Commit sends selected items to the Fleet Baseline.' },
  Conflicts: { icon: TriangleAlert, group: 'This Machine', description: 'A Conflict is a Harness edit that differs from this Machine\'s Draft.' },
  Imports: { icon: ArrowDownToLine, group: 'This Machine', description: 'Available upstream updates. Review each update in the Draft before Commit.' },
  Settings: { icon: SettingsIcon, group: 'Settings', description: 'Harnesses, Fleet membership, and the Daemon connection for this Machine.' },
} as const;
export type SkillsHandoff = { search?: string };
type Screen = keyof typeof screens;
const summaries: Record<Screen, string> = { Fleet: 'Monitor Machines sharing this Baseline.', 'Assignment matrix': 'Choose what each Harness receives.', Log: 'Review Assignment changes.', Library: 'Browse Skills and Rules on this Machine.', Draft: 'Review changes before Commit.', Conflicts: 'Resolve edits made in Harnesses.', Imports: 'Review upstream updates.', Settings: 'Manage this Machine and its connection.' };
const connectionLabel = (connection: StatusView['connection']) => connection.replaceAll('_', ' ');
const sidebarKey = 'sk1ll-sidebar-open';
function storedSidebar() {
  try { const value = localStorage.getItem(sidebarKey); return value === 'true' ? true : value === 'false' ? false : null; } catch { return null; }
}
function Wordmark() {
  return <div className="mb-3 w-44"><img src={wordmarkLight} alt="0xSk1ll" className="block h-auto w-full dark:hidden" /><img src={wordmarkDark} alt="0xSk1ll" className="hidden h-auto w-full dark:block" /></div>;
}

function ConnectionPage({ socket, error }: { socket: string; error: string }) {
  return <main className="relative grid min-h-svh place-items-center bg-muted/40 p-6">
    <div className="absolute top-4 right-4"><ThemeSelector /></div>
    <Card className="w-full max-w-lg">
      <CardHeader><Wordmark /><CardTitle className="text-xl">Daemon is unreachable</CardTitle>
        <CardDescription>Start this Machine's Daemon, then leave this window open. The connection retries automatically.</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-4 text-sm">
        <div className="flex flex-col gap-2"><span className="text-muted-foreground">Start the service</span>
          <pre className="rounded-md border bg-muted px-3 py-2 font-mono text-xs">systemctl --user start sk1ll-daemon</pre></div>
        <div className="flex flex-col gap-2"><span className="text-muted-foreground">Or run it in a terminal</span>
          <pre className="rounded-md border bg-muted px-3 py-2 font-mono text-xs">sk1ll daemon</pre></div>
        <p className="text-xs text-muted-foreground">Socket <code className="font-mono break-all">{socket}</code></p>
        <ErrorNotice error={error} />
        <Badge variant="secondary" role="status"><LoaderCircle className="animate-spin" />Reconnecting…</Badge>
      </CardContent>
    </Card>
  </main>;
}

export function AppSidebar({ screen, setScreen, status, unmanagedCount }: { screen: Screen; setScreen: (screen: Screen) => void; status: StatusView; unmanagedCount: number }) {
  const { isMobile, setOpenMobile } = useSidebar();
  const counts: Partial<Record<Screen, number>> = { Library: unmanagedCount, Draft: status.report.drafts.length, Conflicts: status.report.conflicts.length };
  const item = (name: Screen) => {
    const Icon = screens[name].icon;
    return <SidebarMenuItem key={name}>
      <SidebarMenuButton isActive={screen === name} tooltip={name} aria-current={screen === name ? 'page' : undefined}
        onClick={() => { if (isMobile) setOpenMobile(false); setScreen(name); }}><Icon /><span>{name}</span></SidebarMenuButton>
      {!!counts[name] && <SidebarMenuBadge className={cn(name === 'Conflicts' ? 'bg-warning/13 text-warning-foreground' : 'bg-brand-soft text-brand-ink')} aria-label={name === 'Library' ? `${counts[name]} Unmanaged skill${counts[name] === 1 ? '' : 's'} to adopt` : name === 'Conflicts' ? `${counts[name]} Conflict${counts[name] === 1 ? '' : 's'} to resolve` : `${counts[name]} Draft items`}>{counts[name]}</SidebarMenuBadge>}
    </SidebarMenuItem>;
  };
  const group = (label: string) => (Object.keys(screens) as Screen[]).filter(name => screens[name].group === label);
  return <Sidebar collapsible="icon">
    <SidebarHeader><div className="flex h-12 items-center gap-2 overflow-hidden rounded-md p-2 group-data-[collapsible=icon]:size-8! group-data-[collapsible=icon]:p-0!">
      <img src={symbol} alt="0xSk1ll" className="size-8 shrink-0 object-contain" />
      <div className="grid min-w-0 flex-1 gap-1 text-left text-sm leading-tight group-data-[collapsible=icon]:hidden"><span aria-hidden className="truncate font-semibold">0xSk1ll</span>
        <span className="truncate font-mono text-[11px] text-muted-foreground">{status.machine_id}</span></div>
    </div></SidebarHeader>
    <SidebarContent><nav aria-label="Main navigation" className="contents">
      {['Fleet', 'This Machine'].map(label => <SidebarGroup key={label}><SidebarGroupLabel className="group-data-[collapsible=icon]:pointer-events-none">{label}</SidebarGroupLabel>
        <SidebarGroupContent><SidebarMenu>{group(label).map(item)}</SidebarMenu></SidebarGroupContent></SidebarGroup>)}
      <SidebarGroup className="mt-auto"><SidebarGroupContent><SidebarMenu>{item('Settings')}</SidebarMenu></SidebarGroupContent></SidebarGroup>
    </nav></SidebarContent>
    <SidebarFooter />
    <SidebarRail />
  </Sidebar>;
}

export function App() {
  const [screen, setScreen] = useState<Screen>('Fleet');
  const [paletteOpen, setPaletteOpen] = useState(false);
  const sidebarPreference = useRef(storedSidebar());
  const [sidebarOpen, setSidebarOpen] = useState(() => sidebarPreference.current ?? window.innerWidth >= 1000);
  const changeSidebar = useCallback((open: boolean) => {
    sidebarPreference.current = open; setSidebarOpen(open);
    try { localStorage.setItem(sidebarKey, String(open)); } catch { return; }
  }, []);
  const [skillsHandoff, setSkillsHandoff] = useState<SkillsHandoff>({});
  const [openAdd, setOpenAdd] = useState(false);
  const [matrixTarget, setMatrixTarget] = useState<{ item: string; harness: string }>();
  const [status, setStatus] = useState<StatusView>();
  const [reachable, setReachable] = useState<boolean>();
  const [error, setError] = useState('');
  const [socket, setSocket] = useState('Default Daemon socket');
  const [revision, setRevision] = useState(0);
  const changed = useCallback(() => setRevision(value => value + 1), []);
  const skills = useResource(api.skills, revision);
  const heading = useRef<HTMLHeadingElement>(null);
  const navigated = useRef(false);
  const navigate = useCallback((next: Screen) => { navigated.current = true; setMatrixTarget(undefined); setOpenAdd(false); setScreen(next); }, []);
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
        if (alive) { setStatus(next); setReachable(true); setError(''); }
      } catch (e) {
        if (alive) { if ((e as CommandError)?.unreachable) setReachable(false); setError(errorMessage(e)); }
      } finally {
        refreshing = false;
        if (alive && refreshAgain) { refreshAgain = false; void refresh(); }
      }
    };
    const hint = () => {
      if (eventTimer) clearTimeout(eventTimer);
      eventTimer = setTimeout(() => { changed(); void refresh(); }, 120);
    };
    void api.connectionInfo().then(value => { if (alive) setSocket(value); }).catch(e => { if (alive) setError(errorMessage(e)); });
    void refresh();
    const pollStatus = setInterval(() => void refresh(), 5000);
    const pollFleet = setInterval(changed, 15000);
    const unlisteners: (() => void)[] = [];
    const subscriptions = [
      ...['status', 'draft', 'assignment', 'imports', 'fleet'].map(name => listen(name, hint)),
      listen<DaemonConnection>('daemon-connection', event => {
        if (!alive) return;
        if (!event.payload.reachable) setError(event.payload.message ?? 'Daemon is unreachable');
        hint();
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
  }, [changed]);
  useEffect(() => {
    // Mutations and polling invalidate the machine header as well as screen data.
    let alive = true;
    void api.status().then(value => { if (alive) { setStatus(value); setReachable(true); } }).catch(() => {});
    return () => { alive = false; };
  }, [revision]);
  if (reachable === false) return <TooltipProvider><ConnectionPage socket={socket} error={error} /></TooltipProvider>;
  if (!status) return <main className="grid min-h-svh place-items-center p-6"><div className="flex w-full max-w-sm flex-col gap-3">
    <p className="text-sm text-muted-foreground" role="status">Connecting to the Daemon…</p><Loading /><ErrorNotice error={error} /></div></main>;
  const serverConnected = status.connection === 'connected';
  const conflict = status.report.conflicts[0];
  const server = status.server_url?.replace(/^https?:\/\//, '').replace(/\/$/, '') || 'Server';
  return <ToastProvider><TooltipProvider><SidebarProvider open={sidebarOpen} onOpenChange={changeSidebar}>
    <a href="#main" className="sr-only z-50 rounded-md bg-background px-3 py-2 text-sm shadow focus:not-sr-only focus:fixed focus:top-2 focus:left-2">Skip to content</a>
    <AppSidebar screen={screen} setScreen={navigate} status={status} unmanagedCount={skills.data?.filter(entry => entry.type === 'unmanaged').length ?? 0} />
    <SidebarInset className="min-w-0">
      <header className="sticky top-0 z-10 flex h-12 shrink-0 items-center gap-2 border-b bg-background px-4 md:gap-3 md:px-[18px]">
        <SidebarTrigger className="-ml-1 size-8" />
        <Separator orientation="vertical" className="data-vertical:h-4 data-vertical:self-auto" />
        <span className="min-w-0 truncate text-sm font-semibold">{screen}</span>
        <span className="hidden min-w-0 truncate font-mono text-xs text-muted-foreground xl:block">{status.machine_id} → {server}</span>
        <Button variant="search" className="ml-auto h-8 min-w-0 gap-2 lg:w-full lg:max-w-72" aria-label="Open command palette" onClick={() => setPaletteOpen(true)}>
          <Search data-icon="inline-start" /><span className="hidden truncate lg:inline">Jump to machine, skill, action…</span><kbd className="ml-auto rounded border px-1 font-mono text-[10px]">⌘K</kbd>
        </Button>
        <span className="flex shrink-0 items-center gap-2 text-xs text-muted-foreground" role="status" aria-label={`Server ${connectionLabel(status.connection)}`}>
          <span className={cn('size-2 shrink-0 rounded-full', serverConnected ? 'bg-success' : 'bg-warning')} aria-hidden />
          <span className="hidden sm:inline">{serverConnected ? 'Connected' : connectionLabel(status.connection)}{serverConnected && status.assignment_revision != null && ` · rev ${status.assignment_revision}`}</span>
        </span>
        <Tooltip><TooltipTrigger render={<Button variant="ghost" size="icon" aria-label="Refresh" onClick={changed} />}>
          <RefreshCw /></TooltipTrigger><TooltipContent>Refresh</TooltipContent></Tooltip>
        <ThemeSelector />
      </header>
      {conflict && screen !== 'Conflicts' && <div className="mx-auto mt-3.5 w-full max-w-6xl px-4 md:px-[18px]">
        <Alert variant="warning" role="status" className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg px-3 py-2.5">
          <TriangleAlert /><AlertTitle>This machine has {status.report.conflicts.length === 1 ? 'a conflict' : `${status.report.conflicts.length} conflicts`}</AlertTitle>
          <AlertDescription className="min-w-0 [overflow-wrap:anywhere]"><code className="font-mono">{conflict.item.replace(/^(skill|rule):/, '')}</code> was edited in {conflict.harness} after the last sync.</AlertDescription>
          <Button size="sm" className="ml-auto" onClick={() => navigate('Conflicts')}>Resolve</Button>
        </Alert>
      </div>}
      <main data-library-search={skillsHandoff.search} id="main" className="mx-auto flex w-full max-w-6xl min-w-0 flex-col gap-5 p-4 md:px-[18px] md:py-4">
        <div className="flex flex-col gap-1"><div className="flex items-center gap-2"><h1 ref={heading} tabIndex={-1} className="rounded-sm text-2xl font-semibold tracking-tight outline-none focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-ring">{screen}</h1><Tooltip><TooltipTrigger render={<Button variant="ghost" size="icon-xs" aria-label={`About ${screen}`} />}><Info /></TooltipTrigger><TooltipContent>{screens[screen].description}</TooltipContent></Tooltip></div>
          <p className="text-sm text-muted-foreground">{screen === 'Draft' && status.revoked ? "This Machine's Draft is private. The Daemon has stopped managing its Harnesses until this Machine rejoins." : screen === 'Draft' && status.awaiting_assignment ? "This Machine's Draft is private. Harnesses stay unchanged until the Server sends an Assignment." : summaries[screen]}</p></div>
        {status.revoked && <Notice title="Machine revoked" tone="destructive">This Machine was removed from the Fleet. The Daemon has stopped managing its Harnesses.
          <div><Button variant="outline" size="sm" className="mt-2" onClick={() => navigate('Settings')}>Rejoin in Settings</Button></div></Notice>}
        {status.awaiting_assignment && <Notice title="Awaiting Assignment">The Daemon has no Assignment snapshot. Harnesses are left alone until the Server sends one.</Notice>}
        {status.connection === 'auth_rejected' && <Notice title="Server authentication rejected" tone="destructive">The Daemon is using its cached Assignment and keeps reconnecting.
          {status.last_error && <p className="font-mono text-xs">{status.last_error}</p>}
          <p>If this Machine was removed or its credential rotated, create a join code on another Machine and rejoin.</p>
          <div><Button variant="outline" size="sm" className="mt-2" onClick={() => navigate('Settings')}>Rejoin in Settings</Button></div></Notice>}
        <ErrorNotice error={error} />
        {screen === 'Library' && <><ErrorNotice error={skills.error} />{skills.loading && !skills.data ? <Loading /> : <Skills initialSearch={skillsHandoff.search} connection={status.connection} entries={skills.data ?? []} revision={revision} changed={changed} reviewDraft={() => navigate('Draft')} assignment={(item, harness) => { navigate('Assignment matrix'); setMatrixTarget({ item, harness }); }} addImport={() => { navigate('Imports'); setOpenAdd(true); }} />}</>}
        {screen === 'Fleet' && <Fleet revision={revision} machineId={status.machine_id} localReport={status.report} unmanaged={skills.data?.filter(entry => entry.type === 'unmanaged')} discovery={{ loading: !skills.data && skills.loading, error: skills.error }} navigate={navigate} changed={changed} live={status.connection === 'connected'} />}
        {screen === 'Assignment matrix' && <Matrix revision={revision} status={status} target={matrixTarget} />}
        {screen === 'Draft' && <Draft revision={revision} status={status} changed={changed} />}
        {screen === 'Conflicts' && <Conflicts revision={revision} changed={changed} reviewDraft={() => navigate('Draft')} />}
        {screen === 'Imports' && <Imports openAdd={openAdd} revision={revision} changed={changed} reviewDraft={() => navigate('Draft')} />}
        {screen === 'Log' && <Log revision={revision} changed={changed} />}
        {screen === 'Settings' && <Settings revision={revision} status={status} socket={socket} changed={changed} />}
      </main>
    </SidebarInset>
    <CommandPalette open={paletteOpen} onOpenChange={setPaletteOpen} commands={[
      ...(Object.keys(screens) as Screen[]).map(name => ({ id: `screen-${name.replaceAll(' ', '-')}`, label: name, select: () => navigate(name) })),
      ...(skills.data ?? []).map((entry, index) => ({ id: `content-${index}`, label: `${entry.type === 'rule' ? 'Rule' : 'Skill'}: ${entry.name}`, select: () => { setSkillsHandoff({ search: entry.name }); navigate('Library'); } })),
    ]} />
  </SidebarProvider></TooltipProvider></ToastProvider>;
}
