import { ArrowDownToLine, ChevronDown } from 'lucide-react';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { checkAutomatically, checkForUpdate, installUpdate, progressLabel, setCheckAutomatically, skippedVersions, skipVersion, useAppUpdate } from './updateStore';
import { ErrorNotice, Pending } from './shared';

// `canLeave`: false keeps unsaved edits (the user cancelled the discard prompt).
function UpdateButton({ canLeave, label = 'Update and restart' }: { canLeave: () => boolean; label?: string }) {
  const { progress } = useAppUpdate();
  return <Button size="sm" disabled={!!progress} onClick={() => { if (canLeave()) void installUpdate(); }}>
    <Pending pending={!!progress}>{progress ? progressLabel(progress) : label}</Pending>
  </Button>;
}

function Notes({ notes }: { notes: string | null }) {
  if (!notes?.trim()) return null;
  return <Collapsible>
    <CollapsibleTrigger render={<Button variant="link" size="sm" className="h-auto p-0" />}>Release notes<ChevronDown data-icon="inline-end" /></CollapsibleTrigger>
    <CollapsibleContent><pre className="mt-2 max-h-64 overflow-auto rounded-md border bg-muted px-3 py-2 font-sans text-xs whitespace-pre-wrap">{notes.trim()}</pre></CollapsibleContent>
  </Collapsible>;
}

// The header's notice of a newer release, unless that version was skipped.
export function AppUpdateNotice({ canLeave }: { canLeave: () => boolean }) {
  const { update, progress, error } = useAppUpdate();
  if (!update || (skippedVersions().includes(update.version) && !progress)) return null;
  return <Alert role="status">
    <ArrowDownToLine /><AlertTitle>0xSk1ll {update.version} is available</AlertTitle>
    <AlertDescription className="flex min-w-0 flex-col items-start gap-2">
      <span>This app is {update.current_version}. Updating installs the new release, restarts the Daemon and relaunches the app.</span>
      <Notes notes={update.notes} />
      <div className="flex flex-wrap gap-2"><UpdateButton canLeave={canLeave} />
        {!progress && <Button variant="outline" size="sm" onClick={() => skipVersion(update.version)}>Skip this version</Button>}</div>
      <ErrorNotice error={error} title="Could not update the app" />
    </AlertDescription>
  </Alert>;
}

// What the "This app is out of date" banner offers when the app can update itself.
export function UpdateToServerRelease({ canLeave }: { canLeave: () => boolean }) {
  const { supported, update, checking } = useAppUpdate();
  if (!supported) return null;
  return <div className="mt-2 flex flex-wrap items-center gap-2">
    {update ? <UpdateButton canLeave={canLeave} label={`Update to ${update.version} and restart`} />
      : <Button variant="outline" size="sm" disabled={checking} onClick={() => void checkForUpdate()}><Pending pending={checking}>Check for updates</Pending></Button>}
  </div>;
}

// Settings: check now, turn the startup check on or off, and install.
export function AppUpdateSettings({ canLeave }: { canLeave: () => boolean }) {
  const { supported, checking, checked, update, error } = useAppUpdate();
  if (supported === false) return <p className="text-sm text-muted-foreground">This build can't update itself. Install the release .deb to get updates in the app.</p>;
  return <div className="flex flex-col items-start gap-2 text-sm">
    <label className="flex items-center gap-2"><input type="checkbox" defaultChecked={checkAutomatically()} onChange={event => setCheckAutomatically(event.target.checked)} />Check for updates when the app starts</label>
    <div className="flex flex-wrap items-center gap-2">
      <Button variant="outline" size="sm" disabled={checking} onClick={() => void checkForUpdate()}><Pending pending={checking}>Check now</Pending></Button>
      <span role="status" className="text-muted-foreground">{checking ? 'Checking…' : update ? `${update.version} is available` : checked && !error ? 'Up to date' : ''}</span>
    </div>
    {update && <><Notes notes={update.notes} /><UpdateButton canLeave={canLeave} /></>}
    <ErrorNotice error={error} title="Could not update the app" />
  </div>;
}
