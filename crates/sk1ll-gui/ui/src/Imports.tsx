import { useCallback, useState } from 'react';
import { ChevronRight } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import type { ImportUpdate } from './types';
import { Check, CircleAlert } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { toast } from './toast';
import { api, errorMessage } from './api';
import { AddImport } from './AddImport';
import { useResource } from './hooks';
import { Action, Confirm, ErrorNotice, Loading, Nothing, Panel, PanelToolbar, SectionTitle, When } from './shared';
import { InDraft } from './Conflicts';
import { shortSha } from './model';

const localEdits = (message?: string) => message?.includes('locally modified imports:');

function UpstreamChanges({ update }: { update: ImportUpdate }) {
  const [open, setOpen] = useState(false);
  const load = useCallback(() => api.importChanges(update.name, update.upstream), [update.name, update.upstream]);
  const { data, error, loading } = useResource(load, 0, undefined, open);
  return <Collapsible open={open} onOpenChange={setOpen}>
    <CollapsibleTrigger render={<Button variant="ghost" size="sm" className="group/changes" />}><ChevronRight className="group-data-panel-open/changes:rotate-90" />Upstream changes</CollapsibleTrigger>
    <CollapsibleContent className="flex flex-col gap-2 text-xs break-all">
      {loading ? <p role="status">Loading upstream changes…</p> : <ErrorNotice error={error} />}
      {!loading && !error && data && (data.locked !== update.locked ? <p>Check again to see changes for the current locked commit.</p> : <>
        <p className="font-medium">Commits{data.truncated && ' (first 50)'}</p>
        {data.commits.length ? <ul>{data.commits.map(commit => <li key={commit.sha}><code>{commit.sha}</code> · <time>{commit.date}</time> · {commit.subject}</li>)}</ul> : <p>No upstream commits in this range.</p>}
        <p className="font-medium">Changed files</p>
        {data.files.length ? <ul>{data.files.map(file => <li key={file}><code>{file}</code></li>)}</ul> : <p>No changed files for this Skill.</p>}
      </>)}
    </CollapsibleContent>
  </Collapsible>;
}

export function Imports({ revision, changed, reviewDraft, openAdd = false }: { revision: number; changed: () => void; reviewDraft: () => void; openAdd?: boolean | number }) {
  const { data, setData, error, loading } = useResource(api.imports, revision);
  const [updated, setUpdated] = useState<string>();
  const [refused, setRefused] = useState<Record<string, string>>({});
  const [batchPending, setBatchPending] = useState(false);
  const [results, setResults] = useState<{ name: string; error?: string }[]>([]);
  const updateAll = async () => {
    setBatchPending(true); setResults([]);
    const successes: string[] = [];
    try {
      for (const entry of data?.updates ?? []) {
        try { await api.updateImport(entry.name); successes.push(entry.name); setResults(r => [...r, { name: entry.name }]); }
        catch (e) { setResults(r => [...r, { name: entry.name, error: errorMessage(e) }]); }
      }
      if (successes.length) { setUpdated(successes.join(', ')); changed(); }
    } finally { setBatchPending(false); }
  };
  const rows = data?.imports ?? data?.updates.map(update => ({ ...update, path: '' })) ?? [];
  return <>
    <Panel label="Imports"><PanelToolbar><SectionTitle>Imports</SectionTitle>{data && <p className="text-xs text-muted-foreground">Last checked <When seconds={data.checked_at} /></p>}
      <Action className="h-8" label="Check now" disabled={batchPending} action={async () => setData(await api.checkImports())} />
      {(data?.updates.length ?? 0) >= 2 && <Action label="Update all into Draft" disabled={batchPending} action={updateAll} />}
      <div className="ml-auto"><AddImport initiallyOpen={openAdd} changed={changed} reviewDraft={reviewDraft} /></div></PanelToolbar>
    {results.length > 0 && <ul aria-label="Update results" className="space-y-2 p-4 text-sm">{results.map(result => <li key={result.name} className="flex items-start gap-2">{result.error ? <CircleAlert aria-hidden="true" className="size-4 shrink-0 text-destructive" /> : <Check aria-hidden="true" className="size-4 shrink-0 text-green-600" />}<span>{result.name}: {result.error ? `Failed: ${result.error}` : 'Updated into Draft.'}</span></li>)}</ul>}
    <InDraft name={updated} reviewDraft={reviewDraft} />
    <ErrorNotice error={error} />{loading && !data && <Loading rows={2} />}
    {data && (!rows.length ? <Nothing title="No Imports yet">Add an Import to bring Skills from a git source into the Draft.</Nothing> : <div className="overflow-x-auto"><Table aria-label="Imports">
      <TableHeader><TableRow>{['Import'].map(label => <TableHead key={label}>{label}</TableHead>)}<TableHead className="text-right">Actions</TableHead></TableRow></TableHeader>
      <TableBody>{rows.map(entry => {
        const update = data.updates.find(u => u.name === entry.name), failure = data.errors.find(e => e.name === entry.name), skipped = data.skipped?.find(s => s.name === entry.name);
        const refusal = update && (refused[entry.name] === update.locked || localEdits(results.find(r => r.name === entry.name)?.error));
        const done = () => {
          setUpdated(entry.name);
          setRefused(previous => Object.fromEntries(Object.entries(previous).filter(([name]) => name !== entry.name)));
          setResults(previous => previous.filter(result => result.name !== entry.name));
          changed();
        };
        return <TableRow className="h-14" key={entry.name}><TableCell className="whitespace-normal"><div className="flex flex-wrap items-center gap-2"><span className="font-medium">{entry.name}</span>{failure ? <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1"><Badge variant="destructive">Check failed</Badge><span className="break-words text-xs text-muted-foreground" title={failure.message}>{failure.message}</span></div>
            : update ? <div className="flex flex-wrap items-center gap-x-2 gap-y-1"><Badge variant="info">Update available</Badge><code className="font-mono text-xs text-muted-foreground">{shortSha(update.locked)} → {shortSha(update.upstream)}</code></div>
            : skipped ? <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1"><Badge variant="secondary">Not checked by Server</Badge><span className="break-words text-xs text-muted-foreground" title={skipped.reason}>{skipped.reason}. Check now checks it from this Machine.</span></div>
            : <span className="text-muted-foreground">{data.checked_at === null ? 'Not checked' : 'Up to date'}</span>}</div>
          <p className="mt-1 break-all text-xs text-muted-foreground">Source: {entry.source} · Ref: {entry.ref} · Path: <code>{entry.path || '—'}</code> · Locked commit: <code title={entry.locked ?? undefined}>{shortSha(entry.locked)}</code></p>
          {update && <UpstreamChanges key={`${update.locked}:${update.upstream}`} update={update} />}
          {refusal && <p role="alert" className="mt-1 text-sm text-destructive">This Import has local edits. Fork it to keep them, or overwrite them with the upstream version.</p>}</TableCell>
          <TableCell><div className="flex justify-end gap-2 [&_button]:h-8">{update && !refusal && <Action disabled={batchPending} label="Update into Draft" ariaLabel={`Update ${entry.name} into Draft`} action={async () => {
              try { await api.updateImport(entry.name); done(); toast({ title: `${entry.name} updated into Draft` }); }
              catch (e) {
                if (!localEdits(errorMessage(e))) throw e;
                setRefused(previous => ({ ...previous, [entry.name]: update.locked }));
              }
            }} />}
            {refusal && <>
              <Action disabled={batchPending} label="Fork to keep my edits" action={() => api.fork(entry.name)} onDone={done} toastOnDone={`${entry.name} forked into Draft`} />
              <Confirm disabled={batchPending} label="Overwrite my edits" title={`Overwrite edits to ${entry.name}?`}
                description="This replaces your local edits with the upstream version in this Machine's Draft."
                action={() => api.forceUpdateImport(entry.name)} onDone={done} />
            </>}
            <Confirm disabled={batchPending} label="Remove" triggerVariant="outline" ariaLabel={`Remove ${entry.name}`} title={`Remove ${entry.name}?`} description={`Removes ${entry.name} in this Machine's Draft. Review and Commit the removal to update the Baseline.`}
              action={() => api.removeImport(entry.name)} onDone={() => { setUpdated(`${entry.name} removal`); setData(previous => previous && { ...previous, imports: rows.filter(row => row.name !== entry.name), updates: previous.updates.filter(row => row.name !== entry.name), errors: previous.errors.filter(row => row.name !== entry.name), skipped: previous.skipped?.filter(row => row.name !== entry.name) }); toast({ title: `${entry.name} removal is in the Draft` }); changed(); }} /></div></TableCell></TableRow>;
      })}</TableBody></Table></div>)}
    </Panel>
  </>;
}
