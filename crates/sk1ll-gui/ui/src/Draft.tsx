import { useEffect, useRef, useState } from 'react';
import { ArrowUpFromLine, ChevronRight } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Checkbox } from '@/components/ui/checkbox';
import { Field, FieldLabel, FieldSet, FieldLegend } from '@/components/ui/field';
import { Textarea } from '@/components/ui/textarea';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { Action, ChangeBadge, ErrorNotice, Loading, Nothing, Notice, Pending } from './shared';
import { captureReview, confirmReview, loadDraft, reconcileReview, type DraftReview } from './draftReview';
import type { DraftEntry, PromoteResponse, PullResult, StatusView } from './types';

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
      <code className="font-mono text-sm font-medium">{item}</code><ChangeBadge change={change} />
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
  const [promote, setPromote] = useState<{ review: DraftReview; message: string }>();
  const resetTrigger = useRef<HTMLButtonElement>(null);
  const promoteTrigger = useRef<HTMLButtonElement>(null);
  const completion = useRef<HTMLDivElement>(null);
  const [reset, setReset] = useState<{ review: DraftReview; all: boolean }>();
  const [result, setResult] = useState<PromoteResponse>();
  const [pull, setPull] = useState<PullResult>();
  const mutation = useMutation(changed);
  const prefill = useMutation();
  // Latch changes across refreshes, even if an item later returns to its reviewed content.
  useEffect(() => {
    if (!data) return;
    setPromote(value => value && { ...value, review: reconcileReview(value.review, data) });
    setReset(value => value && { ...value, review: reconcileReview(value.review, data) });
  }, [data]);
  const promoteReview = promote && data && reconcileReview(promote.review, data);
  const resetReview = reset && data && reconcileReview(reset.review, data);
  const reviewDisabled = loading || !!error || mutation.pending;
  const items = selected.filter(item => data?.some(d => d.item === item));
  const all = !!data?.length && items.length === data.length;
  return <>
    {(status.report.blocked || status.report.clashes.length > 0) && <Notice title="Blocked" tone="destructive">
      The new Baseline clashes with this Draft. Promote or Reset the clashing items, then Pull.
      <p className="font-mono text-xs">{status.report.clashes.join(', ')}</p></Notice>}
    <div className="flex flex-wrap items-center gap-2">
      <Button ref={promoteTrigger} disabled={!items.length || prefill.pending || reviewDisabled}
        onClick={() => {
          if (!data) return;
          setResult(undefined);
          const review = captureReview(data, items);
          void prefill.run(() => api.promoteMessage(review.items), value => setPromote({ review, message: value.message }));
        }}>
        <Pending pending={prefill.pending}><ArrowUpFromLine data-icon="inline-start" />Promote {items.length > 0 ? `(${items.length})` : ''}</Pending></Button>
      <Button variant="destructive" disabled={!items.length || reviewDisabled}
        onClick={event => { if (data) { resetTrigger.current = event.currentTarget; setReset({ review: captureReview(data, items), all: false }); } }}>Reset selected</Button>
      <Button variant="ghost" className="text-destructive-foreground hover:text-destructive-foreground" disabled={!data?.length || reviewDisabled}
        onClick={event => { if (data) { resetTrigger.current = event.currentTarget; setReset({ review: captureReview(data, data.map(entry => entry.item), true), all: true }); } }}>Reset all</Button>
      <div className="ml-auto"><Action label="Pull Baseline" action={async () => { const value = await api.pull(); setPull(value); }} onDone={changed} /></div>
    </div><ErrorNotice error={error} /><ErrorNotice error={prefill.error} />
    {pull && <Notice title={pull.state === 'blocked' ? 'Blocked' : 'Pull complete'} tone={pull.state === 'blocked' ? 'destructive' : 'default'}>
      {pull.state === 'blocked' ? `Clashes: ${pull.items.join(', ')}` : pull.state === 'updated' ? `Baseline ${pull.to}` : 'Already on the latest Baseline.'}
    </Notice>}
    {result && <div ref={completion} tabIndex={-1} className="outline-none"><Notice title="Promote complete"><code className="font-mono text-xs">{result.sha}</code>
      <p>{result.pushed ? 'Pushed to the Hub.' : 'Unpushed. The Daemon queued this Promote until it can reach the Hub.'}</p>
      {result.push_error && <p>{result.push_error}</p>}</Notice></div>}
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
    <Dialog open={!!promote} onOpenChange={open => { if (!open && !mutation.pending) setPromote(undefined); }}>
      <DialogContent finalFocus={() => completion.current ?? promoteTrigger.current} showCloseButton={!mutation.pending}><DialogHeader><DialogTitle>Promote to Baseline</DialogTitle>
        <DialogDescription>{promoteReview?.items.length ? `${promoteReview.items.length} Draft ${promoteReview.items.length === 1 ? 'item' : 'items'} selected. Expand an item to review its diff.` : 'No selected items remain in Draft.'}</DialogDescription></DialogHeader>
        {promoteReview && <ReviewNotice review={promoteReview} disabled={reviewDisabled}
          accept={() => setPromote(value => value && { ...value, review: confirmReview(promoteReview) })} />}
        <ErrorNotice error={error} />
        <Field><FieldLabel htmlFor="promote-message">Message</FieldLabel><Textarea id="promote-message"
          value={promote?.message ?? ''} disabled={mutation.pending}
          onChange={e => setPromote(value => value && { ...value, message: e.target.value })} /></Field>
        <ErrorNotice error={mutation.error} /><DialogFooter>
          <Button variant="outline" disabled={mutation.pending} onClick={() => setPromote(undefined)}>Cancel</Button>
          <Button disabled={!promote?.message.trim() || !promoteReview?.items.length || !!promoteReview.changes.length || reviewDisabled} onClick={() => {
            if (promote && promoteReview && !promoteReview.changes.length && !reviewDisabled) void mutation.run(
              () => api.promote({ items: promoteReview.items, message: promote.message }), value => {
              setResult(value); setSelected([]); setPromote(undefined);
            });
          }}><Pending pending={mutation.pending}>Promote</Pending></Button>
        </DialogFooter></DialogContent></Dialog>
    <Dialog open={!!reset} onOpenChange={open => { if (!open && !mutation.pending) setReset(undefined); }}>
      <DialogContent finalFocus={resetTrigger} showCloseButton={!mutation.pending}><DialogHeader>
        <DialogTitle>Reset {reset?.all ? 'reviewed Draft' : 'selected Draft items'}?</DialogTitle>
        <DialogDescription>Return {resetReview?.items.join(', ') || 'no items'} to Baseline. Their Draft changes will be lost.</DialogDescription>
      </DialogHeader>
        {resetReview && <ReviewNotice review={resetReview} disabled={reviewDisabled}
          accept={() => setReset(value => value && { ...value, review: confirmReview(resetReview) })} />}
        <ErrorNotice error={error} /><ErrorNotice error={mutation.error} /><DialogFooter>
          <Button variant="outline" disabled={mutation.pending} onClick={() => setReset(undefined)}>Cancel</Button>
          <Button variant="destructive" disabled={!resetReview?.items.length || !!resetReview.changes.length || reviewDisabled}
            onClick={() => {
              if (resetReview && !resetReview.changes.length && !reviewDisabled) void mutation.run(
                () => api.reset({ items: resetReview.items, all: false }), () => { setReset(undefined); setSelected([]); });
            }}><Pending pending={mutation.pending}>{reset?.all ? 'Reset all' : 'Reset selected'}</Pending></Button>
        </DialogFooter></DialogContent>
    </Dialog>
  </>;
}
