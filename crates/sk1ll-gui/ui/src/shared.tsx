import { useEffect, useState, type ReactNode } from 'react';
import { Check, CircleAlert, Info, Inbox, LoaderCircle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Checkbox } from '@/components/ui/checkbox';
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
import { matrixCheckbox } from './assignments';
import { ago, statusLabels, statusTone, time } from './model';
import type { MachineStatus } from './types';

const changeTone = { added: 'success', modified: 'info', deleted: 'destructive' } as const;
const changeLabels = { added: 'Added', modified: 'Modified', deleted: 'Deleted' };
export function ChangeBadge({ change }: { change: keyof typeof changeTone }) {
  return <Badge variant={changeTone[change]}>{changeLabels[change]}</Badge>;
}
const kindLabels: Record<string, string> = { rule: 'Rule', manifest: 'Manifest', other: 'File' };
export function KindBadge({ kind }: { kind: string }) {
  return <Badge variant="secondary" className="font-sans">{kindLabels[kind] ?? 'Skill'}</Badge>;
}
// Inert copy of an Assignment checkbox, for legends.
export function CheckboxSwatch({ explicit, enabled }: { explicit: boolean; enabled: boolean }) {
  return <span inert aria-hidden className="inline-flex"><Checkbox tabIndex={-1} className={matrixCheckbox(explicit, enabled)} checked={enabled} /></span>;
}
// Unified diff lines whose colours span the full scroll width.
export function DiffLines({ diff }: { diff: string }) {
  return <div className="w-max min-w-full">{diff.split('\n').map((line, i) => <div key={i} className="-mx-3 px-3"
    data-kind={line.startsWith('@@') ? 'hunk' : line.startsWith('+') ? 'add' : line.startsWith('-') ? 'delete' : undefined}>{line || ' '}</div>)}</div>;
}
export function StatusBadge({ status }: { status: MachineStatus }) {
  return <Badge variant={statusTone[status]}><span aria-hidden className="size-1.5 shrink-0 rounded-full bg-current" />{statusLabels[status]}</Badge>;
}
export function ErrorNotice({ error, title = 'Could not complete the request' }: { error?: string; title?: string }) {
  return error ? <Alert variant="destructive" role="alert"><CircleAlert /><AlertTitle>{title}</AlertTitle>
    <AlertDescription className="break-words">{error}</AlertDescription></Alert> : null;
}
export function Notice({ title, children, tone = 'default' }: { title: string; children: ReactNode; tone?: 'default' | 'destructive' }) {
  return <Alert variant={tone} role={tone === 'destructive' ? 'alert' : 'status'}>{tone === 'destructive' ? <CircleAlert /> : <Info />}
    <AlertTitle>{title}</AlertTitle><AlertDescription className={tone === 'destructive' ? 'text-foreground/85!' : undefined}>{children}</AlertDescription></Alert>;
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
// Relative time with the full timestamp available to every user. Dense lists that show the
// exact time visibly pass `focusable={false}` so each row doesn't add a tab stop.
export function When({ seconds, focusable = true }: { seconds: number | null; focusable?: boolean }) {
  if (seconds === null) return <>Never</>;
  const label = <>{ago(seconds)}<span className="sr-only"> ({time(seconds)})</span></>;
  const props = { dateTime: new Date(seconds * 1000).toISOString(), className: 'rounded-sm underline decoration-dotted underline-offset-4 outline-none focus-visible:ring-2 focus-visible:ring-ring' };
  if (!focusable) return <time {...props} title={time(seconds)}>{label}</time>;
  return <Tooltip><TooltipTrigger render={<time {...props} tabIndex={0} />}>{label}</TooltipTrigger><TooltipContent>{time(seconds)}</TooltipContent></Tooltip>;
}
// Screen sections share one heading style: sentence case, semibold, no eyebrow caps.
export function SectionTitle({ id, children, className }: { id?: string; children: ReactNode; className?: string }) {
  return <h2 id={id} className={cn('text-sm font-semibold text-foreground', className)}>{children}</h2>;
}
// A bordered surface whose toolbar sits inside it, above the content. Use for every table or list screen.
export function Panel({ children, className, label }: { children: ReactNode; className?: string; label?: string }) {
  return <section aria-label={label} className={cn('min-w-0 overflow-hidden rounded-xl border bg-card', className)}>{children}</section>;
}
export function PanelToolbar({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn('flex flex-wrap items-center gap-2 border-b px-3.5 py-2.5', className)}>{children}</div>;
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
export function Confirm({ label, title, description, body, action, onDone, disabled = false, typed, ariaLabel, confirmLabel = label, variant = 'destructive', triggerVariant = variant }: {
  label: string; title: string; description: ReactNode; body?: ReactNode; action: () => Promise<unknown>;
  onDone: () => void; disabled?: boolean; typed?: string; ariaLabel?: string; confirmLabel?: string;
  variant?: 'destructive' | 'default'; triggerVariant?: 'destructive' | 'default' | 'outline';
}) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState('');
  const mutation = useMutation();
  return <Dialog open={open} onOpenChange={value => { if (!mutation.pending) { if (value) setText(''); setOpen(value); } }}>
    <DialogTrigger render={<Button variant={triggerVariant} size="sm" aria-label={ariaLabel} disabled={disabled} />}>{label}</DialogTrigger>
    <DialogContent showCloseButton={!mutation.pending}><DialogHeader><DialogTitle>{title}</DialogTitle>
      <DialogDescription>{description}</DialogDescription></DialogHeader>
      {body}
      {typed && <Field><FieldLabel htmlFor="confirm-machine">Type <code className="font-mono">{typed}</code> to confirm</FieldLabel>
        <Input id="confirm-machine" value={text} onChange={e => setText(e.target.value)} autoComplete="off" /></Field>}
      <ErrorNotice error={mutation.error} /><DialogFooter>
        <Button variant="outline" disabled={mutation.pending} onClick={() => setOpen(false)}>Cancel</Button>
        <Button variant={variant} disabled={disabled || mutation.pending || (!!typed && typed !== text)}
          onClick={() => void mutation.run(action, () => { setOpen(false); onDone(); })}>
          <Pending pending={mutation.pending}>{confirmLabel}</Pending></Button>
      </DialogFooter></DialogContent></Dialog>;
}
