import { useState, type ReactNode } from 'react';
import { LoaderCircle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Empty, EmptyHeader, EmptyTitle, EmptyDescription } from '@/components/ui/empty';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Field, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { useMutation } from './hooks';
import { statusLabels, statusTone } from './model';
import type { MachineStatus } from './types';

export function StatusBadge({ status }: { status: MachineStatus }) {
  return <Badge variant={statusTone[status]}>{statusLabels[status]}</Badge>;
}
export function ErrorNotice({ error }: { error?: string }) {
  return error ? <Alert variant="destructive" role="alert"><AlertTitle>Could not complete the request</AlertTitle>
    <AlertDescription>{error}</AlertDescription></Alert> : null;
}
export function Notice({ title, children }: { title: string; children: ReactNode }) {
  return <Alert><AlertTitle>{title}</AlertTitle><AlertDescription>{children}</AlertDescription></Alert>;
}
export function Nothing({ children }: { children: ReactNode }) {
  return <Empty><EmptyHeader><EmptyTitle>No items</EmptyTitle><EmptyDescription>{children}</EmptyDescription>
  </EmptyHeader></Empty>;
}
export function Loading() { return <Skeleton className="h-12 w-full" aria-label="Loading" />; }
export function Pending({ pending, children }: { pending: boolean; children: ReactNode }) {
  return <>{pending && <LoaderCircle data-icon="inline-start" className="animate-spin" />}{children}</>;
}
export function Action({ label, action, onDone, disabled = false }: {
  label: string; action: () => Promise<unknown>; onDone?: () => void; disabled?: boolean;
}) {
  const mutation = useMutation(onDone);
  return <div className="action"><Button variant="outline" size="sm" disabled={disabled || mutation.pending}
    onClick={() => void mutation.run(action)}><Pending pending={mutation.pending}>{label}</Pending></Button>
    <ErrorNotice error={mutation.error} /></div>;
}
export function Confirm({ label, title, description, action, onDone, disabled = false, typed }: {
  label: string; title: string; description: ReactNode; action: () => Promise<unknown>;
  onDone: () => void; disabled?: boolean; typed?: string;
}) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState('');
  const mutation = useMutation();
  return <><Button variant="destructive" size="sm" disabled={disabled} onClick={() => { setText(''); setOpen(true); }}>
    {label}</Button><Dialog open={open} onOpenChange={value => { if (!mutation.pending) setOpen(value); }}>
    <DialogContent showCloseButton={!mutation.pending}><DialogHeader><DialogTitle>{title}</DialogTitle>
      <DialogDescription>{description}</DialogDescription></DialogHeader>
      {typed && <Field><FieldLabel htmlFor="confirm-machine">Type {typed} to confirm</FieldLabel>
        <Input id="confirm-machine" value={text} onChange={e => setText(e.target.value)} autoComplete="off" /></Field>}
      <ErrorNotice error={mutation.error} /><DialogFooter>
        <Button variant="outline" disabled={mutation.pending} onClick={() => setOpen(false)}>Cancel</Button>
        <Button variant="destructive" disabled={disabled || mutation.pending || (!!typed && typed !== text)}
          onClick={() => void mutation.run(action, () => { setOpen(false); onDone(); })}>
          <Pending pending={mutation.pending}>{label}</Pending></Button>
      </DialogFooter></DialogContent></Dialog></>;
}
