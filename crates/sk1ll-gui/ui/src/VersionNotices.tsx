import { useState, type ReactNode } from 'react';
import { TriangleAlert, X } from 'lucide-react';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { api } from './api';
import { useMutation } from './hooks';
import { versionNotices, type VersionNotice } from './model';
import { CopyCommand } from './Setup';
import { ErrorNotice, Notice, Pending } from './shared';
import type { StatusView } from './types';

const dismissedKey = 'sk1ll-dismissed-server-versions';
function dismissed(): string[] {
  try { const stored: unknown = JSON.parse(localStorage.getItem(dismissedKey) ?? '[]'); return Array.isArray(stored) ? stored.filter(v => typeof v === 'string') : []; } catch { return []; }
}

// The command that runs the Server at `version` with the Compose file in this repo (see docs/fleet.md).
const serverUpgrade = (version: string) => `SK1LL_SERVER_VERSION=${version} docker compose -f deploy/docker-compose.server.yml up -d`;

// The header's version warnings. `update`: what this app offers to update itself, when it can.
export function VersionNotices({ status, app, update }: { status: StatusView; app: string; update?: ReactNode }) {
  const [hidden, setHidden] = useState(dismissed);
  const restart = useMutation();
  const notices = versionNotices(status, app).filter(n => n.kind !== 'server-older' || !hidden.includes(n.server));
  const dismiss = (server: string) => {
    const next = [...hidden, server];
    setHidden(next);
    try { localStorage.setItem(dismissedKey, JSON.stringify(next)); } catch { /* Storage may be unavailable. */ }
  };
  const one = (notice: VersionNotice) => {
    switch (notice.kind) {
      case 'server-incompatible': {
        const machine = status.version?.split(' ')[0] ?? '';
        return <Notice key={notice.kind} title="Server is out of date" tone="destructive">
          The Server runs {notice.server ? `sk1ll ${notice.server}` : 'an older release'} and can't talk to this Machine{machine && ` (${machine})`}. The Daemon works from its cached Assignment and keeps reconnecting until the Server is upgraded.
          {machine && <div className="mt-2"><p className="mb-1">On the Server host, in the 0xSk1ll checkout:</p><CopyCommand command={serverUpgrade(machine)} /></div>}
        </Notice>;
      }
      case 'app-incompatible':
        return <Notice key={notice.kind} title="This app is out of date" tone="destructive">
          The Server runs {notice.server ? `sk1ll ${notice.server}` : 'a newer release'} and can't talk to this Machine. The Daemon works from its cached Assignment and keeps reconnecting until this Machine is upgraded.
          {update ?? <p>Install the Server's release of 0xSk1ll on this Machine, then restart the Daemon.</p>}
        </Notice>;
      case 'incompatible':
        return <Notice key={notice.kind} title="Server speaks another protocol" tone="destructive">
          The Daemon works from its cached Assignment and keeps reconnecting. Run the same sk1ll release on the Server and every Machine.
          {status.last_error && <p className="font-mono text-xs">{status.last_error}</p>}
        </Notice>;
      case 'server-older':
        return <Alert key={notice.kind} variant="warning" role="status">
          <TriangleAlert /><AlertTitle>The Server runs an older release</AlertTitle>
          <AlertDescription className="min-w-0">
            The Server runs sk1ll {notice.server} and this Machine {notice.machine}. They still work together; upgrade the Server to get its fixes.
            <div className="mt-2 w-full"><CopyCommand command={serverUpgrade(notice.machine)} /></div>
          </AlertDescription>
          <Button variant="ghost" size="icon" className="absolute top-1.5 right-1.5 size-7" aria-label={`Dismiss for Server ${notice.server}`} onClick={() => dismiss(notice.server)}><X /></Button>
        </Alert>;
      case 'daemon-older':
        return <Alert key={notice.kind} variant="warning" role="status">
          <TriangleAlert /><AlertTitle>The Daemon still runs {notice.daemon}</AlertTitle>
          <AlertDescription className="min-w-0">
            This app is {notice.app}. Restart the Daemon so it runs the same release.
            <div className="mt-2"><Button variant="outline" size="sm" disabled={restart.pending} onClick={() => void restart.run(api.restartDaemon)}><Pending pending={restart.pending}>Restart daemon</Pending></Button></div>
            <ErrorNotice error={restart.error && `${restart.error}. If you started the Daemon in a terminal, stop it there and run sk1ll daemon again.`} />
          </AlertDescription>
        </Alert>;
    }
  };
  return <>{notices.map(one)}</>;
}
