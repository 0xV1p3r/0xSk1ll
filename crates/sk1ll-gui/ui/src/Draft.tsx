import { useEffect, useState } from 'react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Field, FieldLabel, FieldSet, FieldLegend } from '@/components/ui/field';
import { Textarea } from '@/components/ui/textarea';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { api } from './api';
import { useMutation, useResource } from './hooks';
import { Action, ErrorNotice, Loading, Nothing, Notice, Pending } from './shared';
import { captureReview, confirmReview, loadDraft, reconcileReview, type DraftReview } from './draftReview';
import type { PromoteResponse, PullResult, StatusView } from './types';

function Diff({ item, diff }: { item: string; diff: string }) {
  return <pre className="diff" aria-label={`Unified diff for ${item}`}>
    {diff ? diff.split('\n').map((line, i) => <span key={i}
      data-kind={line.startsWith('+') ? 'add' : line.startsWith('-') ? 'delete' : line.startsWith('@@') ? 'hunk' : undefined}>
      {line}{'\n'}</span>) : 'No textual diff.'}</pre>;
}
function DraftItem({ item, change, paths, diff, checked, toggle, disabled }: {
  item: string; change: string; paths: string[]; diff: string; checked: boolean; toggle: () => void; disabled: boolean;
}) {
  return <div className="draft-item"><div className="row"><Checkbox aria-label={`Select ${item}`} checked={checked}
    onCheckedChange={toggle} disabled={disabled} /><code>{item}</code><Badge variant="outline">{change}</Badge></div>
    <details><summary>Diff <span className="meta">{paths.join(', ')}</span></summary><Diff item={item} diff={diff} /></details></div>;
}
function ReviewNotice({ review, disabled, accept }: { review: DraftReview; disabled: boolean; accept: () => void }) {
  return <>{review.changes.length > 0 && <Notice title="Draft changed. Review required">
    <ul>{review.changes.map(change => <li key={change}>{change}</li>)}</ul>
    <p>Review the updated items below. Newly added items are excluded from this action.</p>
    <Button variant="outline" disabled={disabled} onClick={accept}>Review updated Draft</Button>
  </Notice>}{review.snapshot.filter(entry => review.items.includes(entry.item)).map(entry =>
    <details key={entry.item}><summary>{entry.item}</summary><Diff item={entry.item} diff={entry.diff} /></details>)}</>;
}
export function Draft({ revision, status, changed }: { revision: number; status: StatusView; changed: () => void }) {
  const { data, error, loading } = useResource(loadDraft, revision);
  const [selected, setSelected] = useState<string[]>([]);
  const [promote, setPromote] = useState<{ review: DraftReview; message: string }>();
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
  return <><p className="description">This Machine's Draft is live in its Harnesses. Promote sends selected items to the Fleet Baseline.</p>
    {(status.report.blocked || status.report.clashes.length > 0) && <Notice title="Blocked">
      The new Baseline clashes with this Draft. Promote or Reset the clashing items, then Pull.
      <p>{status.report.clashes.join(', ')}</p></Notice>}
    <div className="toolbar"><Button disabled={!items.length || prefill.pending || reviewDisabled}
      onClick={() => {
        if (!data) return;
        const review = captureReview(data, items);
        void prefill.run(() => api.promoteMessage(review.items), value => setPromote({ review, message: value.message }));
      }}>
      <Pending pending={prefill.pending}>Promote {items.length > 0 ? `(${items.length})` : ''}</Pending></Button>
      <Button variant="destructive" size="sm" disabled={!items.length || reviewDisabled}
        onClick={() => data && setReset({ review: captureReview(data, items), all: false })}>Reset selected</Button>
      <Button variant="destructive" size="sm" disabled={!data?.length || reviewDisabled}
        onClick={() => data && setReset({ review: captureReview(data, data.map(entry => entry.item), true), all: true })}>Reset all</Button>
      <Action label="Pull" action={async () => { const value = await api.pull(); setPull(value); }} onDone={changed} />
    </div><ErrorNotice error={error} /><ErrorNotice error={prefill.error} />
    {pull && <Notice title={pull.state === 'blocked' ? 'Blocked' : 'Pull complete'}>
      {pull.state === 'blocked' ? `Clashes: ${pull.items.join(', ')}` : pull.state === 'updated' ? `Baseline ${pull.to}` : 'Already on the latest Baseline.'}
    </Notice>}
    {result && <Notice title="Promote complete"><code>{result.sha}</code>
      <p>{result.pushed ? 'Pushed to the Hub.' : 'Unpushed. The Daemon queued this Promote until it can reach the Hub.'}</p>
      {result.push_error && <p>{result.push_error}</p>}</Notice>}
    {loading && !data && <Loading />}{data?.length === 0 && <Nothing>This Machine has no Draft changes.</Nothing>}
    {!!data?.length && <FieldSet><FieldLegend>Draft items</FieldLegend>
      <div className="toolbar"><Button variant="ghost" size="sm" disabled={mutation.pending}
        onClick={() => setSelected(items.length === data.length ? [] : data.map(d => d.item))}>
        {items.length === data.length ? 'Clear checkboxes' : 'Check all items'}</Button></div>
      {data.map(entry => <DraftItem key={entry.item} {...entry} checked={items.includes(entry.item)}
        disabled={mutation.pending} toggle={() => setSelected(previous => previous.includes(entry.item)
          ? previous.filter(i => i !== entry.item) : [...previous, entry.item])} />)}
    </FieldSet>}
    <Dialog open={!!promote} onOpenChange={open => { if (!open && !mutation.pending) setPromote(undefined); }}>
      <DialogContent showCloseButton={!mutation.pending}><DialogHeader><DialogTitle>Promote to Baseline</DialogTitle>
        <DialogDescription>{promoteReview?.items.join(', ') || 'No selected items remain in Draft.'}</DialogDescription></DialogHeader>
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
      <DialogContent showCloseButton={!mutation.pending}><DialogHeader>
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
