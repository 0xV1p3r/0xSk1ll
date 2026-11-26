import { useEffect, useRef, useState } from 'react';
import { ArrowUpFromLine, ChevronRight } from 'lucide-react';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Checkbox } from '@/components/ui/checkbox';
import { Field, FieldLabel, FieldSet, FieldLegend } from '@/components/ui/field';
import { Textarea } from '@/components/ui/textarea';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Badge } from '@/components/ui/badge';
import { toast } from './toast';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { plural, pullToast } from './model';
import { Action, ChangeBadge, DiffLines, ErrorNotice, KindBadge, Loading, Nothing, Notice, Panel, PanelToolbar, Pending } from './shared';
import { captureReview, confirmReview, loadDraft, reconcileReview, type DraftReview } from './draftReview';
import type { DraftEntry, CommitResponse, PullKeepResult, PullResult, StatusView } from './types';

function Diff({ item, diff }: { item: string; diff: string }) {
  return <pre tabIndex={0} className="diff mt-2 min-w-0 max-w-full max-h-[32rem] overflow-auto rounded-md border bg-muted/40 p-3 font-mono text-xs leading-relaxed outline-none [tab-size:4] focus-visible:ring-3 focus-visible:ring-ring"
    aria-label={`Unified diff for ${item}`}>
    {diff ? <DiffLines diff={diff} /> : <span className="text-muted-foreground">No textual diff.</span>}</pre>;
}
function DraftItem({ item, change, paths, diff, checked, toggle, disabled, open, onOpenChange, unpushed }: {
  item: string; change: DraftEntry['change']; paths: string[]; diff: string; checked: boolean; toggle: () => void; disabled: boolean; open: boolean; onOpenChange: (open: boolean) => void; unpushed: boolean;
}) {
  const additions = diff.split('\n').filter(line => line.startsWith('+') && !line.startsWith('+++')).length, deletions = diff.split('\n').filter(line => line.startsWith('-') && !line.startsWith('---')).length;
  return <Collapsible open={open} onOpenChange={onOpenChange} className="min-w-0 max-w-full px-4 py-3">
    <div className="flex flex-wrap items-center gap-3">
      <Checkbox aria-label={`Select ${item}`} checked={checked} onCheckedChange={toggle} disabled={disabled} />
      <span className="text-sm font-medium">{item.replace(/^(skill|rule|other):/, '')}</span><KindBadge kind={item === 'manifest' ? 'manifest' : item.split(':')[0]} /><ChangeBadge change={change} />{unpushed && <Badge variant="secondary">Unpushed</Badge>}<span className="text-xs tabular-nums" aria-label={`+${additions} / −${deletions}`}><span className={additions ? 'text-success-foreground' : 'text-muted-foreground'}>+{additions}</span>{' / '}<span className={deletions ? 'text-destructive-foreground' : 'text-muted-foreground'}>−{deletions}</span></span>
      <span className="min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground" title={paths.join(', ')}>{paths.join(', ')}</span>
      <CollapsibleTrigger render={<Button variant="ghost" size="default" className="group/diff" aria-label={`Diff for ${item}`} />}>
        <ChevronRight className="transition-transform group-data-panel-open/diff:rotate-90" />Diff</CollapsibleTrigger>
    </div>
    <CollapsibleContent className="min-w-0 max-w-full"><Diff item={item} diff={diff} /></CollapsibleContent>
  </Collapsible>;
}
function ReviewNotice({ review, disabled, accept }: { review: DraftReview; disabled: boolean; accept: () => void }) {
  const [expanded, setExpanded] = useState<string[]>([]);
  const entries = review.snapshot.filter(entry => review.items.includes(entry.item));
  const allExpanded = entries.length > 0 && entries.every(entry => expanded.includes(entry.item));
  return <><Button variant="outline" onClick={() => setExpanded(allExpanded ? [] : entries.map(entry => entry.item))}>{allExpanded ? 'Collapse all' : 'Expand all diffs'}</Button>{review.changes.length > 0 && <Notice title="Draft changed. Review required" tone="destructive">
    <ul className="list-disc pl-4">{review.changes.map(change => <li key={change}>{change}</li>)}</ul>
    <p>Review the updated items below. Newly added items are excluded from this action.</p>
    <div><Button variant="outline" size="default" className="mt-1" disabled={disabled} onClick={accept}>Review updated Draft</Button></div>
  </Notice>}{entries.map(entry =>
    <Collapsible key={entry.item} open={expanded.includes(entry.item)} onOpenChange={open => setExpanded(previous => open ? [...previous, entry.item] : previous.filter(item => item !== entry.item))} className="min-w-0 max-w-full"><CollapsibleTrigger render={<Button variant="ghost" size="default" className="group/diff -ml-2" />}>
      <ChevronRight className="transition-transform group-data-panel-open/diff:rotate-90" /><span>{entry.item}</span>
    </CollapsibleTrigger><CollapsibleContent className="min-w-0 max-w-full"><Diff item={entry.item} diff={entry.diff} /></CollapsibleContent></Collapsible>)}</>;
}
export function Draft({ revision, status, changed }: { revision: number; status: StatusView; changed: () => void }) {
  const standalone = status.mode === 'standalone';
  const noOrigin = standalone && !status.has_origin;
  const [pushAfterCommit, setPushAfterCommit] = useState(false);
  const { data, error, loading } = useResource(loadDraft, revision);
  const [selected, setSelected] = useState<string[]>([]);
  const selectionTouched = useRef(false);
  const [expanded, setExpanded] = useState<string[]>([]);
  const [commit, setCommit] = useState<{ review: DraftReview; message: string }>();
  const restoreTrigger = useRef<HTMLButtonElement>(null);
  const commitTrigger = useRef<HTMLButtonElement>(null);
  const completion = useRef<HTMLDivElement>(null);
  const [restore, setRestore] = useState<{ review: DraftReview; all: boolean }>();
  const [result, setResult] = useState<CommitResponse>();
  const [resultHidden, setResultHidden] = useState(false);
  const [pull, setPull] = useState<PullResult>();
  const [keep, setKeep] = useState<{ items: string[]; all: boolean }>();
  const [kept, setKept] = useState<PullKeepResult>();
  const keepTrigger = useRef<HTMLButtonElement>(null);
  const mutation = useMutation(changed);
  const prefill = useMutation();
  const push = useMutation(changed);
  const pushNow = () => void push.run(api.push, value => { if (!value.pushed) throw new Error('Push did not reach the Server.'); setResult(previous => previous && { ...previous, pushed: true, push_error: null }); toast({ title: 'Push complete' }); });
  // Latch changes across refreshes, even if an item later returns to its reviewed content.
  useEffect(() => {
    if (!data) return;
    setCommit(value => value && { ...value, review: reconcileReview(value.review, data) });
    setRestore(value => value && { ...value, review: reconcileReview(value.review, data) });
  }, [data]);
  const commitReview = commit && data && reconcileReview(commit.review, data);
  const restoreReview = restore && data && reconcileReview(restore.review, data);
  const restoreCount = restoreReview?.items.length ?? 0;
  const reviewDisabled = loading || !!error || mutation.pending;
  const items = (selectionTouched.current ? selected : data?.map(entry => entry.item) ?? []).filter(item => data?.some(d => d.item === item));
  const all = !!data?.length && items.length === data.length;
  const blocked = status.report.blocked || status.report.clashes.length > 0 || pull?.state === 'blocked';
  const clashes = pull?.state === 'blocked' ? pull.items : status.report.clashes;
  const keepNames = keep?.all ? clashes.join(', ') : keep?.items.join(', ');
  const allExpanded = !!data?.length && data.every(entry => expanded.includes(entry.item));
  const pullAction = noOrigin ? <Button variant="outline" disabled>Pull</Button> : <Action label="Pull Baseline" action={async () => { const value = await api.pull(); setPull(value); setKept(undefined); toast(pullToast(value)); }} onDone={changed} />;
  const unpushed = [...new Set([...status.report.unpushed, ...(!result?.pushed ? result?.items ?? [] : [])])];
  return <>
    {!!status.keep_report?.length && <Notice title="Interrupted Keep mine" tone="destructive">
      An interrupted Keep mine left your versions in Recovery. Adopt each one you want back in Conflicts → Recovered content, or run the command shown.
      <ul className="flex flex-col gap-1" aria-label="Kept versions in recovery">{status.keep_report.map(u => <li key={`${u.item}:${u.entry ?? u.error}`} className="text-xs">
        <span className="font-mono">{u.item}</span>: {u.error}{u.entry && <> <code className="font-mono">sk1ll recover adopt {u.entry}</code></>}</li>)}</ul>
      {status.keep_report.some(u => !u.entry) && <div><Action label="Dismiss notes" action={api.dismissKeepNotes} onDone={changed} /></div>}</Notice>}
    {blocked && <Notice title="Blocked" tone="destructive">
      The new Baseline also changes these items, which this Machine changed too. <strong>Keep mine</strong> pulls the new Baseline and puts your version back on top as Draft: the whole file wins, so the other Machine's change to it is overwritten. <strong>Restore</strong> takes the Baseline's version instead (yours is kept in Recovery), then Pull.
      <ul className="flex flex-col gap-1" aria-label="Clashing items">{clashes.map(item => <li key={item} className="flex flex-wrap items-center gap-2">
        <span className="min-w-0 flex-1 truncate font-mono text-xs">{item}</span>
        <Button size="sm" variant="outline" disabled={noOrigin || mutation.pending} aria-label={`Keep mine: ${item}`} onClick={event => { keepTrigger.current = event.currentTarget; setKeep({ items: [item], all: false }); }}>Keep mine</Button>
        <Button size="sm" variant="outline" className="enabled:text-destructive-foreground" aria-label={`Restore ${item}`} disabled={!data?.some(entry => entry.item === item) || reviewDisabled}
          onClick={event => { if (data) { restoreTrigger.current = event.currentTarget; setRestore({ review: captureReview(data, [item]), all: false }); } }}>Restore</Button>
      </li>)}</ul>
      <div className="flex flex-wrap gap-2">{clashes.length > 1 && <Button size="sm" disabled={noOrigin || mutation.pending} onClick={event => { keepTrigger.current = event.currentTarget; setKeep({ items: [], all: true }); }}>Keep mine for all</Button>}{pullAction}</div></Notice>}
    {kept && !!kept.kept?.length && <Notice title="Kept your version">
      Your version of {kept.kept.join(', ')} is back on top of {kept.pull.state === 'updated' ? `Baseline ${kept.pull.to}` : 'the new Baseline'} as Draft, replacing the other Machine's change. Review its diff below (or run <code className="font-mono text-xs">sk1ll diff {kept.kept.join(' ')}</code>), then Commit.
      <div><Button variant="ghost" onClick={() => setKept(undefined)}>Dismiss</Button></div></Notice>}
    <Panel label="Draft"><PanelToolbar>
      {standalone && <><Button variant="outline" disabled={noOrigin || push.pending} onClick={pushNow}>Push</Button>{pullAction}{noOrigin && <span className="text-xs text-muted-foreground">Push and Pull require an origin git remote.</span>}</>}
      {data?.length !== 0 && <><Button ref={commitTrigger} disabled={!items.length || prefill.pending || reviewDisabled}
        onClick={() => {
          if (!data) return;
          setResult(undefined); setResultHidden(false);
          const review = captureReview(data, items);
          void prefill.run(() => api.commitMessage(review.items), value => setCommit({ review, message: value.message }));
        }}>
        <Pending pending={prefill.pending}><ArrowUpFromLine data-icon="inline-start" />Commit {items.length > 0 ? `(${items.length})` : ''}</Pending></Button>
      <Button variant="outline" className="enabled:text-destructive-foreground" disabled={!items.length || reviewDisabled}
        onClick={event => { if (data) { restoreTrigger.current = event.currentTarget; setRestore({ review: captureReview(data, items), all: false }); } }}>Restore selected</Button></>}
      {!!data?.length && <><Button variant="outline" className="enabled:text-destructive-foreground" disabled={reviewDisabled} onClick={event => { restoreTrigger.current = event.currentTarget; setRestore({ review: captureReview(data, data.map(entry => entry.item), true), all: true }); }}>Restore all</Button>
      <Button variant="outline" onClick={() => setExpanded(allExpanded ? [] : data.map(entry => entry.item))}>{allExpanded ? 'Collapse all' : 'Expand all diffs'}</Button></>}
      {status.report.behind && !blocked && <div className="ml-auto">{pullAction}</div>}
    </PanelToolbar><ErrorNotice error={error} /><ErrorNotice error={prefill.error} />
    <ErrorNotice error={push.error} />
    {unpushed.length > 0 && <div ref={result && !result.pushed ? completion : undefined} tabIndex={-1} className="px-3.5 pt-3 outline-none"><Alert role="status" className="border-info/30 bg-brand-soft px-3 py-2.5"><ArrowUpFromLine /><AlertTitle>{unpushed.length} unpushed {unpushed.length === 1 ? 'item' : 'items'}</AlertTitle><AlertDescription className="flex flex-wrap items-center gap-x-3 gap-y-2"><div className="min-w-0 flex-1"><ul className="mt-1 flex flex-wrap gap-1" aria-label="Unpushed items">{unpushed.map(item => <li className="max-w-full truncate rounded border bg-background/40 px-1.5 py-0.5 font-mono text-xs text-foreground/85" key={item} title={item}>{item.replace(/^(skill|rule|other):/, '')}</li>)}</ul>{result && !result.pushed && !resultHidden && <><p>Commit complete: <code className="font-mono text-xs">{result.sha}</code></p>{result.push_error && <p>{result.push_error}</p>}<Button variant="ghost" onClick={() => setResultHidden(true)}>Dismiss commit result</Button></>}</div><Button size="sm" disabled={noOrigin || push.pending} onClick={pushNow}>Push now</Button></AlertDescription></Alert></div>}
    {pull && pull.state !== 'blocked' && !kept?.kept?.length && <Notice title="Pull complete">{pull.state === 'updated' ? `Baseline ${pull.to}` : 'Already on the latest Baseline.'}</Notice>}
    {result?.pushed && !resultHidden && <div ref={completion} tabIndex={-1} className="outline-none"><Notice title="Commit complete"><code className="font-mono text-xs">{result.sha}</code><p>{standalone ? 'Pushed to origin.' : 'Pushed to the Server.'}</p><Button variant="ghost" onClick={() => setResultHidden(true)}>Dismiss</Button></Notice></div>}
    {loading && !data && <Loading />}{data?.length === 0 && <Nothing title="Draft is clean">This Machine has no Draft changes.</Nothing>}
    {!!data?.length && <FieldSet className="min-w-0 w-full max-w-full gap-0"><FieldLegend className="sr-only">Draft items</FieldLegend>
      <div className="divide-y divide-border">
        <div className="flex items-center gap-3 bg-muted/50 px-4 py-2 text-sm">
          <Checkbox aria-label={all ? 'Clear checkboxes' : 'Check all items'} checked={all}
            indeterminate={items.length > 0 && !all} disabled={mutation.pending}
            onCheckedChange={() => { selectionTouched.current = true; setSelected(all ? [] : data.map(d => d.item)); }} />
          <span className="text-muted-foreground">{items.length ? `${items.length} of ${data.length} selected` : plural(data.length, 'Draft item')}</span>
        </div>
        {data.map(entry => <DraftItem key={entry.item} {...entry} checked={items.includes(entry.item)}
          open={expanded.includes(entry.item)} onOpenChange={open => setExpanded(previous => open ? [...previous, entry.item] : previous.filter(item => item !== entry.item))} unpushed={unpushed.includes(entry.item)}
          disabled={mutation.pending} toggle={() => { selectionTouched.current = true; setSelected(items.includes(entry.item) ? items.filter(i => i !== entry.item) : [...items, entry.item]); }} />)}
      </div>
    </FieldSet>}</Panel>
    <Dialog open={!!commit} onOpenChange={open => { if (!open && !mutation.pending) setCommit(undefined); }}>
      <DialogContent finalFocus={() => completion.current ?? commitTrigger.current} showCloseButton={!mutation.pending}><DialogHeader><DialogTitle>{standalone ? 'Commit to local repository' : 'Commit to Baseline'}</DialogTitle>
        <DialogDescription>{commitReview?.items.length ? `${commitReview.items.length} Draft ${commitReview.items.length === 1 ? 'item' : 'items'} selected. Expand an item to review its diff.` : 'No selected items remain in Draft.'}</DialogDescription></DialogHeader>
        {commitReview && <ReviewNotice review={commitReview} disabled={reviewDisabled}
          accept={() => setCommit(value => value && { ...value, review: confirmReview(commitReview) })} />}
        <ErrorNotice error={error} />
        <Field><FieldLabel htmlFor="commit-message">Message</FieldLabel><Textarea id="commit-message"
          value={commit?.message ?? ''} disabled={mutation.pending}
          onChange={e => setCommit(value => value && { ...value, message: e.target.value })} /></Field>
        {standalone && status.has_origin && <Field orientation="horizontal"><Checkbox id="push-after-commit" checked={pushAfterCommit} onCheckedChange={setPushAfterCommit} disabled={mutation.pending} /><FieldLabel htmlFor="push-after-commit">Push after commit</FieldLabel></Field>}
        <ErrorNotice error={mutation.error} /><DialogFooter>
          <Button variant="outline" disabled={mutation.pending} onClick={() => setCommit(undefined)}>Cancel</Button>
          <Button disabled={!commit?.message.trim() || !commitReview?.items.length || !!commitReview.changes.length || reviewDisabled} onClick={() => {
            if (commit && commitReview && !commitReview.changes.length && !reviewDisabled) void mutation.run(
              async () => { const value = await api.commit({ items: commitReview.items, message: commit.message }); if (standalone && status.has_origin && pushAfterCommit) { try { const pushed = await api.push(); return { ...value, pushed: pushed.pushed, push_error: pushed.pushed ? null : 'Push did not reach origin.' }; } catch (e) { return { ...value, push_error: String(e) }; } } return value; }, value => {
              toast({ title: 'Commit complete' }); setResult(value); selectionTouched.current = true; setSelected([]); setCommit(undefined);
            });
          }}><Pending pending={mutation.pending}>Commit</Pending></Button>
        </DialogFooter></DialogContent></Dialog>
    <Dialog open={!!keep} onOpenChange={open => { if (!open && !mutation.pending) setKeep(undefined); }}>
      <DialogContent finalFocus={keepTrigger} showCloseButton={!mutation.pending}><DialogHeader>
        <DialogTitle>Keep your version of {keep?.all ? 'every clashing item' : keepNames}?</DialogTitle>
        <DialogDescription>Pull the new Baseline, then put your version of {keepNames} back on top as a Draft change. The whole file wins: the other Machine's change to {keep?.all || (keep?.items.length ?? 0) > 1 ? 'these items' : 'this item'} is overwritten in your Draft. Review the diff (or <code className="font-mono text-xs">sk1ll diff</code>) before you Commit.</DialogDescription>
      </DialogHeader>
        <ErrorNotice error={mutation.error} /><DialogFooter>
          <Button variant="outline" disabled={mutation.pending} onClick={() => setKeep(undefined)}>Cancel</Button>
          <Button variant="destructive" disabled={mutation.pending} onClick={() => {
            if (keep) void mutation.run(() => api.pullKeep(keep), value => {
              setPull(value.pull); setKept(value); setKeep(undefined);
              if (value.kept?.length) { setExpanded(previous => [...new Set([...previous, ...value.kept ?? []])]); toast({ title: 'Kept your version' }); }
              else toast(pullToast(value.pull));
            });
          }}><Pending pending={mutation.pending}>Keep mine</Pending></Button>
        </DialogFooter></DialogContent>
    </Dialog>
    <Dialog open={!!restore} onOpenChange={open => { if (!open && !mutation.pending) setRestore(undefined); }}>
      <DialogContent finalFocus={restoreTrigger} showCloseButton={!mutation.pending}><DialogHeader>
        <DialogTitle>Restore {restore?.all ? 'all Draft items' : plural(restoreCount, 'selected item')}?</DialogTitle>
        <DialogDescription>Return {restoreReview?.items.join(', ') || 'no items'} to Baseline. Draft changes to {restoreCount === 1 ? 'this item' : 'these items'} will be lost.</DialogDescription>
      </DialogHeader>
        {restoreReview && <ReviewNotice review={restoreReview} disabled={reviewDisabled}
          accept={() => setRestore(value => value && { ...value, review: confirmReview(restoreReview) })} />}
        <ErrorNotice error={error} /><ErrorNotice error={mutation.error} /><DialogFooter>
          <Button variant="outline" disabled={mutation.pending} onClick={() => setRestore(undefined)}>Cancel</Button>
          <Button variant="destructive" disabled={!restoreReview?.items.length || !!restoreReview.changes.length || reviewDisabled}
            onClick={() => {
              if (restoreReview && !restoreReview.changes.length && !reviewDisabled) void mutation.run(
                () => api.restore({ items: restoreReview.items, all: false }), () => { toast({ title: 'Restore complete' }); setRestore(undefined); setSelected([]); });
            }}><Pending pending={mutation.pending}>{restore?.all ? 'Restore all' : 'Restore selected'}</Pending></Button>
        </DialogFooter></DialogContent>
    </Dialog>
  </>;
}
