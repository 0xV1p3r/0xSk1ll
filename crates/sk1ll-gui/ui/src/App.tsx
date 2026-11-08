import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { ArrowDownToLine, BookOpen, FilePen, History, LayoutGrid, LoaderCircle, RefreshCw, Server, Settings as SettingsIcon, TriangleAlert } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Separator } from '@/components/ui/separator';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupContent, SidebarGroupLabel, SidebarHeader, SidebarInset,
  SidebarMenu, SidebarMenuBadge, SidebarMenuButton, SidebarMenuItem, SidebarProvider, SidebarRail, SidebarTrigger, useSidebar,
} from '@/components/ui/sidebar';
import { ThemeSelector } from '@/components/theme-selector';
import { api, errorMessage } from './api';
import { Fleet } from './Fleet';
import { Matrix } from './Matrix';
import { Draft } from './Draft';
import { Conflicts, Imports, Log } from './Activity';
import { Skills } from './Skills';
import { useResource } from './hooks';
import { Settings } from './Settings';
import { ErrorNotice, Loading, Notice, StatusBadge } from './shared';
import type { CommandError, DaemonConnection, StatusView } from './types';

const screens = {
  Fleet: { icon: Server, group: 'Fleet', description: 'Machines sharing this Baseline. Reports refresh live and every 15 seconds.' },
  'Assignment matrix': { icon: LayoutGrid, group: 'Fleet', description: 'Assignment takes effect immediately. Explicit toggles override a Harness default.' },
  Log: { icon: History, group: 'Fleet', description: 'Assignment log, newest first. Undo restores an entry\'s previous value and adds a new log entry.' },
  Skills: { icon: BookOpen, group: 'This Machine', description: 'Browse Personal Skills, Imports, Fragments, and Unmanaged skills on this Machine.' },
  Draft: { icon: FilePen, group: 'This Machine', description: 'This Machine\'s Draft is live in its Harnesses. Promote sends selected items to the Fleet Baseline.' },
  Conflicts: { icon: TriangleAlert, group: 'This Machine', description: 'A Conflict is a Harness edit that differs from this Machine\'s Draft.' },
  Imports: { icon: ArrowDownToLine, group: 'This Machine', description: 'Available upstream updates. Review each update in the Draft before Promote.' },
  Settings: { icon: SettingsIcon, group: 'Settings', description: 'Harnesses, Fleet membership, and the Daemon connection for this Machine.' },
} as const;
type Screen = keyof typeof screens;
const connectionLabel = (connection: StatusView['connection']) => connection.replaceAll('_', ' ');

function ConnectionPage({ socket, error }: { socket: string; error: string }) {
  return <main className="relative grid min-h-svh place-items-center bg-muted/40 p-6">
    <div className="absolute top-4 right-4"><ThemeSelector /></div>
    <Card className="w-full max-w-lg">
      <CardHeader><CardDescription>0xSk1ll</CardDescription><CardTitle className="text-xl">Daemon is unreachable</CardTitle>
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
  const counts: Partial<Record<Screen, number>> = { Skills: unmanagedCount, Draft: status.report.drafts.length, Conflicts: status.report.conflicts.length };
  const item = (name: Screen) => {
    const Icon = screens[name].icon;
    return <SidebarMenuItem key={name}>
      <SidebarMenuButton isActive={screen === name} tooltip={name} aria-current={screen === name ? 'page' : undefined}
        onClick={() => { if (isMobile) setOpenMobile(false); setScreen(name); }}><Icon /><span>{name}</span></SidebarMenuButton>
      {!!counts[name] && <SidebarMenuBadge aria-label={name === 'Skills' ? `${counts[name]} Unmanaged skills` : undefined}>{counts[name]}</SidebarMenuBadge>}
    </SidebarMenuItem>;
  };
  const group = (label: string) => (Object.keys(screens) as Screen[]).filter(name => screens[name].group === label);
  return <Sidebar collapsible="icon">
    <SidebarHeader><div className="flex h-12 items-center gap-2 overflow-hidden rounded-md p-2 group-data-[collapsible=icon]:size-8! group-data-[collapsible=icon]:p-0!">
      <div className="flex aspect-square size-8 shrink-0 items-center justify-center rounded-lg bg-primary font-mono text-xs font-semibold text-primary-foreground">0x</div>
      <div className="grid flex-1 text-left text-sm leading-tight"><span className="truncate font-semibold">0xSk1ll</span>
        <span className="truncate text-xs text-muted-foreground">{status.machine_id}</span></div>
    </div></SidebarHeader>
    <SidebarContent><nav aria-label="Main navigation" className="contents">
      {['Fleet', 'This Machine'].map(label => <SidebarGroup key={label}><SidebarGroupLabel>{label}</SidebarGroupLabel>
        <SidebarGroupContent><SidebarMenu>{group(label).map(item)}</SidebarMenu></SidebarGroupContent></SidebarGroup>)}
      <SidebarGroup className="mt-auto"><SidebarGroupContent><SidebarMenu>{item('Settings')}</SidebarMenu></SidebarGroupContent></SidebarGroup>
    </nav></SidebarContent>
    <SidebarFooter />
    <SidebarRail />
  </Sidebar>;
}

export function App() {
  const [screen, setScreen] = useState<Screen>('Fleet');
  const [status, setStatus] = useState<StatusView>();
  const [reachable, setReachable] = useState<boolean>();
  const [error, setError] = useState('');
  const [socket, setSocket] = useState('Default Daemon socket');
  const [revision, setRevision] = useState(0);
  const changed = useCallback(() => setRevision(value => value + 1), []);
  const skills = useResource(api.skills, revision);
  const heading = useRef<HTMLHeadingElement>(null);
  const navigated = useRef(false);
  const navigate = useCallback((next: Screen) => { navigated.current = true; setScreen(next); }, []);
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
  const hubConnected = status.connection === 'connected';
  return <TooltipProvider><SidebarProvider>
    <a href="#main" className="sr-only z-50 rounded-md bg-background px-3 py-2 text-sm shadow focus:not-sr-only focus:fixed focus:top-2 focus:left-2">Skip to content</a>
    <AppSidebar screen={screen} setScreen={navigate} status={status} unmanagedCount={skills.data?.filter(entry => entry.type === 'unmanaged').length ?? 0} />
    <SidebarInset className="min-w-0">
      <header className="sticky top-0 z-10 flex h-14 shrink-0 items-center gap-2 border-b bg-background/95 px-4 backdrop-blur">
        <SidebarTrigger className="-ml-1" />
        <Separator orientation="vertical" className="mr-2 data-vertical:h-4 data-vertical:self-auto" />
        <span className="truncate text-sm font-medium">{status.machine_id}</span>
        <StatusBadge status={status.headline} />
        <div className="ml-auto flex items-center gap-2">
          <Badge variant="outline" className="hidden sm:inline-flex">
            <span className={hubConnected ? 'size-1.5 rounded-full bg-success' : 'size-1.5 rounded-full bg-warning'} aria-hidden />
            Hub {connectionLabel(status.connection)}
          </Badge>
          <Tooltip><TooltipTrigger render={<Button variant="ghost" size="icon" aria-label="Refresh" onClick={changed} />}>
            <RefreshCw /></TooltipTrigger><TooltipContent>Refresh</TooltipContent></Tooltip>
          <ThemeSelector />
        </div>
      </header>
      <main id="main" className="mx-auto flex w-full max-w-6xl min-w-0 flex-col gap-6 p-4 md:p-6">
        <div className="flex flex-col gap-1"><h1 ref={heading} tabIndex={-1} className="rounded-sm text-2xl font-semibold tracking-tight outline-none focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-ring">{screen}</h1>
          <p className="text-sm text-muted-foreground">{screens[screen].description}</p></div>
        {status.revoked && <Notice title="Machine revoked" tone="destructive">This Machine was removed from the Fleet. The Daemon has stopped managing its Harnesses.
          <div><Button variant="outline" size="sm" className="mt-2" onClick={() => navigate('Settings')}>Rejoin in Settings</Button></div></Notice>}
        {status.awaiting_assignment && <Notice title="Awaiting Assignment">The Daemon has no Assignment snapshot. Harnesses are left alone until the Hub sends one.</Notice>}
        {status.connection === 'auth_rejected' && <Notice title="Hub authentication rejected" tone="destructive">The Daemon is using its cached Assignment and keeps reconnecting.
          {status.last_error && <p className="font-mono text-xs">{status.last_error}</p>}
          <p>If this Machine was removed or its credential rotated, create a join code on another Machine and rejoin.</p>
          <div><Button variant="outline" size="sm" className="mt-2" onClick={() => navigate('Settings')}>Rejoin in Settings</Button></div></Notice>}
        <ErrorNotice error={error} />
        {screen === 'Skills' && <><ErrorNotice error={skills.error} />{skills.loading && !skills.data ? <Loading /> : <Skills connection={status.connection} entries={skills.data ?? []} revision={revision} changed={changed} reviewDraft={() => navigate('Draft')} assignment={() => navigate('Assignment matrix')} />}</>}
        {screen === 'Fleet' && <Fleet revision={revision} machineId={status.machine_id} navigate={navigate} changed={changed} live={status.connection === 'connected'} />}
        {screen === 'Assignment matrix' && <Matrix revision={revision} status={status} />}
        {screen === 'Draft' && <Draft revision={revision} status={status} changed={changed} />}
        {screen === 'Conflicts' && <Conflicts revision={revision} changed={changed} reviewDraft={() => navigate('Draft')} />}
        {screen === 'Imports' && <Imports revision={revision} changed={changed} reviewDraft={() => navigate('Draft')} />}
        {screen === 'Log' && <Log revision={revision} changed={changed} />}
        {screen === 'Settings' && <Settings revision={revision} status={status} socket={socket} changed={changed} />}
      </main>
    </SidebarInset>
  </SidebarProvider></TooltipProvider>;
}
