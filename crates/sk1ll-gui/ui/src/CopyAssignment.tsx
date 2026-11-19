import { useId, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle } from '@/components/ui/dialog';
import { Field, FieldGroup, FieldLabel } from '@/components/ui/field';
import { NativeSelect } from '@/components/ui/native-select';
import { api, errorMessage, errorStatus } from './api';
import { ErrorNotice } from './shared';
import type { AssignmentCopyPreview, AssignmentUpdate, Expected, MachineAssignmentCopy } from './types';

interface Props {
  target: string;
  expected: Expected;
  targetHarnesses: string[];
  sources: { id: string; harnesses: string[] }[];
  close: () => void;
  refreshed?: () => Promise<void>;
  copied: (update: AssignmentUpdate) => void;
}

export function CopyAssignment({ target, expected, targetHarnesses, sources, close, copied, refreshed }: Props) {
  const id = useId();
  const [latest, setLatest] = useState<{ expected: Expected; harnesses: string[] }>();
  const currentHarnesses = latest?.harnesses ?? targetHarnesses;
  const [source, setSource] = useState('');
  const [mode, setMode] = useState<MachineAssignmentCopy['mode']>('replace');
  const [mapping, setMapping] = useState<Record<string, string>>({});
  const [preview, setPreview] = useState<{ result: AssignmentCopyPreview; body: MachineAssignmentCopy }>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const sourceHarnesses = sources.find(machine => machine.id === source)?.harnesses ?? [];
  const fromFor = (to: string) => mapping[to] !== undefined ? (sourceHarnesses.includes(mapping[to]) ? mapping[to] : '') : (sourceHarnesses.includes(to) ? to : '');
  const reset = () => { setPreview(undefined); setError(undefined); };

  const submit = async () => {
    setBusy(true);
    setError(undefined);
    const body: MachineAssignmentCopy = {
      source, mode, expected: latest?.expected ?? expected,
      harnesses: currentHarnesses.flatMap(to => {
        const from = fromFor(to);
        return from ? [{ from, to }] : [];
      }),
    };
    try {
      if (preview) copied(await api.copyMachineAssignment(target, {
        ...preview.body, expected: preview.result.target, source_expected: preview.result.source,
      }));
      else setPreview({ result: await api.previewAssignmentCopy(target, body), body });
    } catch (error) {
      setError(errorMessage(error));
      setPreview(undefined);
      if (errorStatus(error) === 409) {
        try {
          const snapshot = await api.assignment(target);
          await refreshed?.();
          setLatest({ expected: { revision: snapshot.revision, registration: snapshot.registration },
            harnesses: Object.keys(snapshot.assignment.harnesses).sort() });
          setMapping({});
        } catch (reloadError) {
          setError(errorMessage(reloadError));
        }
      }
    } finally {
      setBusy(false);
    }
  };

  return <Dialog open onOpenChange={open => { if (!open && !busy) close(); }}>
    <DialogContent showCloseButton={!busy}>
      <DialogTitle>Copy Assignment from…</DialogTitle>
      <DialogDescription>Copy to {target}. Replace copies defaults and explicit toggles. Merge fills unset toggles and keeps the target defaults.</DialogDescription>
      <FieldGroup className="gap-4">
      <Field><FieldLabel htmlFor={`${id}-source`}>Source Machine</FieldLabel>
        <NativeSelect id={`${id}-source`} value={source} disabled={busy} onChange={event => { setSource(event.target.value); setMapping({}); reset(); }}>
          <option value="">Select a Machine</option>
          {sources.map(machine => <option key={machine.id}>{machine.id}</option>)}
        </NativeSelect>
      </Field>
      <Field><FieldLabel htmlFor={`${id}-mode`}>Mode</FieldLabel>
        <NativeSelect id={`${id}-mode`} value={mode} disabled={busy} onChange={event => { setMode(event.target.value === 'merge' ? 'merge' : 'replace'); reset(); }}>
          <option value="replace">Replace</option><option value="merge">Merge</option>
        </NativeSelect>
      </Field>
      {source && currentHarnesses.map(to => <Field key={to}><FieldLabel htmlFor={`${id}-harness-${to}`}>Source Harness for {to}</FieldLabel>
        <NativeSelect id={`${id}-harness-${to}`} value={fromFor(to)} disabled={busy} onChange={event => {
          setMapping(old => ({ ...old, [to]: event.target.value }));
          reset();
        }}>
          <option value="">Skip {to}</option>
          {sourceHarnesses.map(from => <option key={from}>{from}</option>)}
        </NativeSelect>
      </Field>)}
      </FieldGroup>
      {preview && <p role="status">{preview.result.count} {preview.result.count === 1 ? 'change' : 'changes'} to item toggles and defaults</p>}
      <ErrorNotice error={error} />
      <DialogFooter>
        <Button variant="outline" disabled={busy} onClick={close}>Cancel</Button>
        <Button disabled={busy || !source || !currentHarnesses.some(fromFor) || preview?.result.count === 0}
          onClick={() => void submit()}>{preview ? 'Copy Assignment' : 'Preview changes'}</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>;
}
