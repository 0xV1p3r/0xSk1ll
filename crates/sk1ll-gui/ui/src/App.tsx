import { useCallback, useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { TooltipProvider } from '@/components/ui/tooltip';
import { Separator } from '@/components/ui/separator';
import { api, errorMessage } from './api';
import { Fleet } from './Fleet';
import { Matrix } from './Matrix';
import { Draft } from './Draft';
import { Conflicts, Imports, Log } from './Activity';
import { Settings } from './Settings';
import { ErrorNotice, Loading, Notice, StatusBadge } from './shared';
import { cn } from '@/lib/utils';
import type { CommandError, DaemonConnection, StatusView } from './types';

const screens = ['Fleet', 'Assignment matrix', 'Draft', 'Conflicts', 'Imports', 'Log', 'Settings'] as const;
type Screen = typeof screens[number];
type Theme = 'system' | 'light' | 'dark';
export function App() {
  const [screen, setScreen] = useState<Screen>('Fleet');
  const [status, setStatus] = useState<StatusView>();
  const [reachable, setReachable] = useState<boolean>();
  const [error, setError] = useState('');
  const [socket, setSocket] = useState('Default Daemon socket');
  const [revision, setRevision] = useState(0);
  const [theme, setTheme] = useState<Theme>(() => {
    const saved = localStorage.getItem('sk1ll-theme');
    return saved === 'light' || saved === 'dark' ? saved : 'system';
  });
  const changed = useCallback(() => setRevision(value => value + 1), []);
  useEffect(() => {
    const preference = window.matchMedia('(prefers-color-scheme: dark)');
    const apply = () => document.documentElement.classList.toggle('dark', theme === 'dark' || (theme === 'system' && preference.matches));
    apply(); localStorage.setItem('sk1ll-theme', theme);
    preference.addEventListener('change', apply);
    return () => preference.removeEventListener('change', apply);
  }, [theme]);
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
  const themeControl = <label className="theme-control">Appearance <select aria-label="Appearance" value={theme}
    onChange={event => setTheme(event.target.value as Theme)}>
    <option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select></label>;
  if (reachable === false) return <TooltipProvider><main className="connection-page"><div className="connection-content">
    <p className="app-name">0xSk1ll</p><h1>Daemon is unreachable</h1>
    <p>Start this Machine's Daemon, then leave this window open. The connection retries automatically.</p>
    <pre>systemctl --user start sk1ll-daemon</pre><p>Or run it in a terminal:</p><pre>sk1ll daemon</pre>
    <p className="meta">{socket}</p><ErrorNotice error={error} /><Badge variant="secondary">Reconnecting…</Badge>
    {themeControl}</div></main></TooltipProvider>;
  if (!status) return <main className="connection-page"><div className="connection-content"><h1>Connecting to the Daemon…</h1>
    <Loading /><ErrorNotice error={error} /></div></main>;
  return <TooltipProvider><div className="app-shell"><aside><div className="brand">0xSk1ll</div>
    <nav aria-label="Main navigation">{screens.map(name => <button key={name} aria-current={screen === name ? 'page' : undefined}
      className={cn('nav-item', screen === name && 'active')} onClick={() => setScreen(name)}>{name}</button>)}</nav>
    <div className="sidebar-bottom">{themeControl}</div></aside><div className="workspace"><header>
      <div className="row"><strong>{status.machine_id}</strong><StatusBadge status={status.headline} /></div>
      <div className="row"><Badge variant="outline">Daemon connected</Badge><span className="meta">Hub {status.connection.replaceAll('_', ' ')}</span></div>
    </header><Separator /><main className="screen"><div className="screen-heading"><h1>{screen}</h1>
      <Button variant="ghost" size="sm" onClick={changed}>Refresh</Button></div>
      {status.revoked && <Notice title="Machine revoked">This Machine was removed from the Fleet. The Daemon has stopped managing its Harnesses.
        <Button variant="link" onClick={() => setScreen('Settings')}>Rejoin in Settings</Button></Notice>}
      {status.awaiting_assignment && <Notice title="Awaiting Assignment">The Daemon has no Assignment snapshot. Harnesses are left alone until the Hub sends one.</Notice>}
      {status.connection === 'auth_rejected' && <Notice title="Hub authentication rejected">The Daemon is using its cached Assignment and keeps reconnecting.
        {status.last_error && <p>{status.last_error}</p>}</Notice>}
      <ErrorNotice error={error} />
      {screen === 'Fleet' && <Fleet revision={revision} />}
      {screen === 'Assignment matrix' && <Matrix revision={revision} status={status} />}
      {screen === 'Draft' && <Draft revision={revision} status={status} changed={changed} />}
      {screen === 'Conflicts' && <Conflicts revision={revision} changed={changed} />}
      {screen === 'Imports' && <Imports revision={revision} changed={changed} />}
      {screen === 'Log' && <Log revision={revision} changed={changed} />}
      {screen === 'Settings' && <Settings revision={revision} status={status} socket={socket} changed={changed} />}
    </main></div></div></TooltipProvider>;
}
