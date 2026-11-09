import { useEffect, useState, type ReactNode } from 'react';
import { Check, CircleAlert, Info, Inbox, LoaderCircle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Empty, EmptyContent, EmptyHeader, EmptyMedia, EmptyTitle, EmptyDescription } from '@/components/ui/empty';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter, DialogTrigger } from '@/components/ui/dialog';
import { Field, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';
import { toast } from './toast';
import { useMutation } from './hooks';
import { ago, statusLabels, statusTone, time } from './model';
import type { MachineStatus } from './types';

const changeTone = { added: 'success', modified: 'info', deleted: 'destructive' } as const;
const changeLabels = { added: 'Added', modified: 'Modified', deleted: 'Deleted' };
export function ChangeBadge({ change }: { change: keyof typeof changeTone }) {
  return <Badge variant={changeTone[change]}>{changeLabels[change]}</Badge>;
}
export function StatusBadge({ status }: { status: MachineStatus }) {
  return <Badge variant={statusTone[status]}>{statusLabels[status]}</Badge>;
}
export function ErrorNotice({ error, title = 'Could not complete the request' }: { error?: string; title?: string }) {
  return error ? <Alert variant="destructive" role="alert"><CircleAlert /><AlertTitle>{title}</AlertTitle>
    <AlertDescription className="break-words">{error}</AlertDescription></Alert> : null;
}
export function Notice({ title, children, tone = 'default' }: { title: string; children: ReactNode; tone?: 'default' | 'destructive' }) {
  return <Alert variant={tone} role={tone === 'destructive' ? 'alert' : 'status'}>{tone === 'destructive' ? <CircleAlert /> : <Info />}
    <AlertTitle>{title}</AlertTitle><AlertDescription>{children}</AlertDescription></Alert>;
}
export function Nothing({ title = 'Nothing here', children, className, action }: { title?: string; children: ReactNode; className?: string; action?: ReactNode }) {
  return <Empty className={cn('border border-dashed', className)}><EmptyHeader><EmptyMedia variant="icon"><Inbox /></EmptyMedia>
    <EmptyTitle>{title}</EmptyTitle><EmptyDescription>{children}</EmptyDescription></EmptyHeader>{action && <EmptyContent>{action}</EmptyContent>}</Empty>;
}
export function Loading({ rows = 3 }: { rows?: number }) {
  return <div className="flex flex-col gap-2" role="status" aria-label="Loading">
    {Array.from({ length: rows }, (_, i) => <Skeleton key={i} className="h-10 w-full" />)}</div>;
}
export function Pending({ pending, children }: { pending: boolean; children: ReactNode }) {
  return <>{pending && <LoaderCircle data-icon="inline-start" className="animate-spin" />}{children}</>;
}
// A small titled block of key/value rows.
export function Details({ rows, className }: { rows: [string, ReactNode][]; className?: string }) {
  return <dl className={cn('grid grid-cols-[max-content_minmax(0,1fr)] gap-x-6 gap-y-2 text-sm', className)}>
    {rows.map(([term, value]) => <div key={term} className="contents"><dt className="text-muted-foreground">{term}</dt>
      <dd className="break-words">{value}</dd></div>)}</dl>;
}
// Relative time with the full timestamp available to every user.
export function When({ seconds }: { seconds: number | null }) {
  if (seconds === null) return <>Never</>;
  return <Tooltip><TooltipTrigger render={<time dateTime={new Date(seconds * 1000).toISOString()} tabIndex={0}
    className="rounded-sm underline decoration-dotted underline-offset-4 outline-none focus-visible:ring-2 focus-visible:ring-ring" />}>
    {ago(seconds)}<span className="sr-only"> ({time(seconds)})</span></TooltipTrigger><TooltipContent>{time(seconds)}</TooltipContent></Tooltip>;
}
export function Action({ label, action, onDone, disabled = false, variant = 'outline', ariaLabel, doneLabel, className, announceDone = true, toastOnDone }: {
  label: string; action: () => Promise<unknown>; onDone?: () => void; disabled?: boolean;
  variant?: 'outline' | 'default' | 'ghost' | 'secondary'; ariaLabel?: string; doneLabel?: string; className?: string; announceDone?: boolean; toastOnDone?: string;
}) {
  const mutation = useMutation(onDone);
  const [done, setDone] = useState(false);
  useEffect(() => {
    if (!done) return;
    const timer = setTimeout(() => setDone(false), 2500);
    return () => clearTimeout(timer);
  }, [done]);
  return <div className="flex min-w-0 flex-col items-start gap-2">
    <Button variant={variant} size="sm" className={className} aria-label={ariaLabel} disabled={disabled} aria-disabled={disabled || mutation.pending}
      onClick={() => { if (!disabled && !mutation.pending) void mutation.run(action, () => { setDone(!!doneLabel); if (toastOnDone) toast({ title: toastOnDone }); }); }}>
      {done ? <><Check data-icon="inline-start" />{doneLabel}</> : <Pending pending={mutation.pending}>{label}</Pending>}</Button>
    {announceDone && <span role="status" className="sr-only">{done ? doneLabel : ''}</span>}
    <ErrorNotice error={mutation.error} /></div>;
}
export function Confirm({ label, title, description, action, onDone, disabled = false, typed, ariaLabel, confirmLabel = label, variant = 'destructive' }: {
  label: string; title: string; description: ReactNode; action: () => Promise<unknown>;
  onDone: () => void; disabled?: boolean; typed?: string; ariaLabel?: string; confirmLabel?: string;
  variant?: 'destructive' | 'default';
}) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState('');
  const mutation = useMutation();
  return <Dialog open={open} onOpenChange={value => { if (!mutation.pending) { if (value) setText(''); setOpen(value); } }}>
    <DialogTrigger render={<Button variant={variant} size="sm" aria-label={ariaLabel} disabled={disabled} />}>{label}</DialogTrigger>
    <DialogContent showCloseButton={!mutation.pending}><DialogHeader><DialogTitle>{title}</DialogTitle>
      <DialogDescription>{description}</DialogDescription></DialogHeader>
      {typed && <Field><FieldLabel htmlFor="confirm-machine">Type <code className="font-mono">{typed}</code> to confirm</FieldLabel>
        <Input id="confirm-machine" value={text} onChange={e => setText(e.target.value)} autoComplete="off" /></Field>}
      <ErrorNotice error={mutation.error} /><DialogFooter>
        <Button variant="outline" disabled={mutation.pending} onClick={() => setOpen(false)}>Cancel</Button>
        <Button variant={variant} disabled={disabled || mutation.pending || (!!typed && typed !== text)}
          onClick={() => void mutation.run(action, () => { setOpen(false); onDone(); })}>
          <Pending pending={mutation.pending}>{confirmLabel}</Pending></Button>
      </DialogFooter></DialogContent></Dialog>;
}
