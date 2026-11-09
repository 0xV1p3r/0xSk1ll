import { useEffect, useRef, useState } from 'react';
import { ArrowUpFromLine, ChevronRight } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Checkbox } from '@/components/ui/checkbox';
import { Field, FieldLabel, FieldSet, FieldLegend } from '@/components/ui/field';
import { Textarea } from '@/components/ui/textarea';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Badge } from '@/components/ui/badge';
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { toast } from './toast';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { Action, ChangeBadge, ErrorNotice, Loading, Nothing, Notice, Pending } from './shared';
import { captureReview, confirmReview, loadDraft, reconcileReview, type DraftReview } from './draftReview';
import type { DraftEntry, CommitResponse, PullResult, StatusView } from './types';

function Diff({ item, diff }: { item: string; diff: string }) {
  return <pre tabIndex={0} className="diff mt-2 min-w-0 max-w-full max-h-[32rem] overflow-auto rounded-md border bg-muted/40 p-3 font-mono text-xs leading-relaxed outline-none [tab-size:4] focus-visible:ring-3 focus-visible:ring-ring"
    aria-label={`Unified diff for ${item}`}>
    {diff ? diff.split('\n').map((line, i) => <span key={i}
      data-kind={line.startsWith('+') ? 'add' : line.startsWith('-') ? 'delete' : line.startsWith('@@') ? 'hunk' : undefined}>
      {line}{'\n'}</span>) : <span className="text-muted-foreground">No textual diff.</span>}</pre>;
}
function DraftItem({ item, change, paths, diff, checked, toggle, disabled }: {
  item: string; change: DraftEntry['change']; paths: string[]; diff: string; checked: boolean; toggle: () => void; disabled: boolean;
}) {
  return <Collapsible className="min-w-0 max-w-full px-4 py-3">
    <div className="flex flex-wrap items-center gap-3">
      <Checkbox aria-label={`Select ${item}`} checked={checked} onCheckedChange={toggle} disabled={disabled} />
      <code className="font-mono text-sm font-medium">{item.replace(/^(skill|rule):/, '')}</code><Badge variant="outline">{item.startsWith('rule:') ? 'Rule' : 'Skill'}</Badge><ChangeBadge change={change} /><span className="font-mono text-xs">+{diff.split('\n').filter(line => line.startsWith('+') && !line.startsWith('+++')).length} / −{diff.split('\n').filter(line => line.startsWith('-') && !line.startsWith('---')).length}</span>
      <span className="min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground" title={paths.join(', ')}>{paths.join(', ')}</span>
      <CollapsibleTrigger render={<Button variant="ghost" size="sm" className="group/diff" aria-label={`Diff for ${item}`} />}>
        <ChevronRight className="transition-transform group-data-panel-open/diff:rotate-90" />Diff</CollapsibleTrigger>
    </div>
    <CollapsibleContent className="min-w-0 max-w-full"><Diff item={item} diff={diff} /></CollapsibleContent>
  </Collapsible>;
}
function ReviewNotice({ review, disabled, accept }: { review: DraftReview; disabled: boolean; accept: () => void }) {
  return <>{review.changes.length > 0 && <Notice title="Draft changed. Review required" tone="destructive">
    <ul className="list-disc pl-4">{review.changes.map(change => <li key={change}>{change}</li>)}</ul>
    <p>Review the updated items below. Newly added items are excluded from this action.</p>
    <div><Button variant="outline" size="sm" className="mt-1" disabled={disabled} onClick={accept}>Review updated Draft</Button></div>
  </Notice>}{review.snapshot.filter(entry => review.items.includes(entry.item)).map(entry =>
    <Collapsible key={entry.item} className="min-w-0 max-w-full"><CollapsibleTrigger render={<Button variant="ghost" size="sm" className="group/diff -ml-2" />}>
      <ChevronRight className="transition-transform group-data-panel-open/diff:rotate-90" /><code className="font-mono">{entry.item}</code>
    </CollapsibleTrigger><CollapsibleContent className="min-w-0 max-w-full"><Diff item={entry.item} diff={entry.diff} /></CollapsibleContent></Collapsible>)}</>;
}
export function Draft({ revision, status, changed }: { revision: number; status: StatusView; changed: () => void }) {
  const { data, error, loading } = useResource(loadDraft, revision);
  const [selected, setSelected] = useState<string[]>([]);
  const [commit, setCommit] = useState<{ review: DraftReview; message: string }>();
  const restoreTrigger = useRef<HTMLButtonElement>(null);
  const commitTrigger = useRef<HTMLButtonElement>(null);
  const completion = useRef<HTMLDivElement>(null);
  const [restore, setRestore] = useState<{ review: DraftReview; all: boolean }>();
  const [result, setResult] = useState<CommitResponse>();
  const [pull, setPull] = useState<PullResult>();
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
  const reviewDisabled = loading || !!error || mutation.pending;
  const items = selected.filter(item => data?.some(d => d.item === item));
  const all = !!data?.length && items.length === data.length;
  return <>
    {(status.report.blocked || status.report.clashes.length > 0) && <Notice title="Blocked" tone="destructive">
      The new Baseline clashes with this Draft. Commit or Restore the clashing items, then Pull.
      <p className="font-mono text-xs">{status.report.clashes.join(', ')}</p></Notice>}
    <div className="flex flex-wrap items-center gap-2">
      <Button ref={commitTrigger} disabled={!items.length || prefill.pending || reviewDisabled}
        onClick={() => {
          if (!data) return;
          setResult(undefined);
          const review = captureReview(data, items);
          void prefill.run(() => api.commitMessage(review.items), value => setCommit({ review, message: value.message }));
        }}>
        <Pending pending={prefill.pending}><ArrowUpFromLine data-icon="inline-start" />Commit {items.length > 0 ? `(${items.length})` : ''}</Pending></Button>
      <Button variant="outline" className="text-destructive-foreground" disabled={!items.length || reviewDisabled}
        onClick={event => { if (data) { restoreTrigger.current = event.currentTarget; setRestore({ review: captureReview(data, items), all: false }); } }}>Restore selected</Button>
      <div className="ml-auto"><Action label="Pull Baseline" action={async () => { const value = await api.pull(); setPull(value); toast({ title: value.state === 'blocked' ? 'Pull blocked' : 'Pull complete' }); }} onDone={changed} /></div>
      <DropdownMenu><DropdownMenuTrigger render={<Button variant="outline" ref={restoreTrigger} onClick={event => { restoreTrigger.current = event.currentTarget; }} />}>More</DropdownMenuTrigger><DropdownMenuContent align="end"><DropdownMenuGroup><DropdownMenuItem variant="destructive" disabled={!data?.length || reviewDisabled}
        onClick={() => { if (data) setRestore({ review: captureReview(data, data.map(entry => entry.item), true), all: true }); }}>Restore all</DropdownMenuItem></DropdownMenuGroup></DropdownMenuContent></DropdownMenu>
    </div><ErrorNotice error={error} /><ErrorNotice error={prefill.error} />
    <ErrorNotice error={push.error} />
    {status.report.unpushed.length > 0 && <Notice title={`${status.report.unpushed.length} unpushed`}><ul>{status.report.unpushed.map(item => <li key={item}>{item.replace(/^(skill|rule):/, '')}</li>)}</ul><Button variant="outline" disabled={push.pending} onClick={pushNow}>Push now</Button></Notice>}
    {pull && <Notice title={pull.state === 'blocked' ? 'Blocked' : 'Pull complete'} tone={pull.state === 'blocked' ? 'destructive' : 'default'}>
      {pull.state === 'blocked' ? `Clashes: ${pull.items.join(', ')}` : pull.state === 'updated' ? `Baseline ${pull.to}` : 'Already on the latest Baseline.'}
    </Notice>}
    {result && <div ref={completion} tabIndex={-1} className="outline-none"><Notice title="Commit complete"><code className="font-mono text-xs">{result.sha}</code>
      <p>{result.pushed ? 'Pushed to the Server.' : 'Unpushed. The Daemon queued this Commit until it can reach the Server.'}</p>
      {!result.pushed && <Button variant="outline" disabled={push.pending} onClick={pushNow}>Retry push</Button>}{result.push_error && <p>{result.push_error}</p>}</Notice></div>}
    {loading && !data && <Loading />}{data?.length === 0 && <Nothing title="Draft is clean">This Machine has no Draft changes.</Nothing>}
    {!!data?.length && <FieldSet className="min-w-0 w-full max-w-full gap-0"><FieldLegend className="sr-only">Draft items</FieldLegend>
      <div className="divide-y overflow-hidden rounded-lg border">
        <div className="flex items-center gap-3 bg-muted/50 px-4 py-2 text-sm">
          <Checkbox aria-label={all ? 'Clear checkboxes' : 'Check all items'} checked={all}
            indeterminate={items.length > 0 && !all} disabled={mutation.pending}
            onCheckedChange={() => setSelected(all ? [] : data.map(d => d.item))} />
          <span className="text-muted-foreground">{items.length ? `${items.length} of ${data.length} selected` : `${data.length} Draft items`}</span>
        </div>
        {data.map(entry => <DraftItem key={entry.item} {...entry} checked={items.includes(entry.item)}
          disabled={mutation.pending} toggle={() => setSelected(previous => previous.includes(entry.item)
            ? previous.filter(i => i !== entry.item) : [...previous, entry.item])} />)}
      </div>
    </FieldSet>}
    <Dialog open={!!commit} onOpenChange={open => { if (!open && !mutation.pending) setCommit(undefined); }}>
      <DialogContent finalFocus={() => completion.current ?? commitTrigger.current} showCloseButton={!mutation.pending}><DialogHeader><DialogTitle>Commit to Baseline</DialogTitle>
        <DialogDescription>{commitReview?.items.length ? `${commitReview.items.length} Draft ${commitReview.items.length === 1 ? 'item' : 'items'} selected. Expand an item to review its diff.` : 'No selected items remain in Draft.'}</DialogDescription></DialogHeader>
        {commitReview && <ReviewNotice review={commitReview} disabled={reviewDisabled}
          accept={() => setCommit(value => value && { ...value, review: confirmReview(commitReview) })} />}
        <ErrorNotice error={error} />
        <Field><FieldLabel htmlFor="commit-message">Message</FieldLabel><Textarea id="commit-message"
          value={commit?.message ?? ''} disabled={mutation.pending}
          onChange={e => setCommit(value => value && { ...value, message: e.target.value })} /></Field>
        <ErrorNotice error={mutation.error} /><DialogFooter>
          <Button variant="outline" disabled={mutation.pending} onClick={() => setCommit(undefined)}>Cancel</Button>
          <Button disabled={!commit?.message.trim() || !commitReview?.items.length || !!commitReview.changes.length || reviewDisabled} onClick={() => {
            if (commit && commitReview && !commitReview.changes.length && !reviewDisabled) void mutation.run(
              () => api.commit({ items: commitReview.items, message: commit.message }), value => {
              toast({ title: 'Commit complete' }); setResult(value); setSelected([]); setCommit(undefined);
            });
          }}><Pending pending={mutation.pending}>Commit</Pending></Button>
        </DialogFooter></DialogContent></Dialog>
    <Dialog open={!!restore} onOpenChange={open => { if (!open && !mutation.pending) setRestore(undefined); }}>
      <DialogContent finalFocus={restoreTrigger} showCloseButton={!mutation.pending}><DialogHeader>
        <DialogTitle>Restore {restore?.all ? 'reviewed Draft' : 'selected Draft items'}?</DialogTitle>
        <DialogDescription>Return {restoreReview?.items.join(', ') || 'no items'} to Baseline. Their Draft changes will be lost.</DialogDescription>
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
