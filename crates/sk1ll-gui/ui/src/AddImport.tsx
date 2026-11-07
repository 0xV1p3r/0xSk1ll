import { useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Field, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { api, errorMessage } from './api';
import { ErrorNotice } from './shared';
import { shortSha } from './model';
import type { ResolvedSource } from './types';

export function AddImport({ changed, reviewDraft }: {
  changed: () => void; reviewDraft: () => void;
}) {
  const [open, setOpen] = useState(false);
  const busy = useRef(false);
  return <><Button onClick={() => setOpen(true)}>Add</Button>
    <Dialog open={open} onOpenChange={value => { if (!busy.current) setOpen(value); }}>{open && <AddImportForm busy={busy} changed={changed}
      reviewDraft={() => { setOpen(false); reviewDraft(); }} />}</Dialog></>;
}
function AddImportForm({ busy, changed, reviewDraft }: {
  busy: { current: boolean }; changed: () => void; reviewDraft: () => void;
}) {
  const [link, setLink] = useState('');
  const [path, setPath] = useState('');
  const [resolved, setResolved] = useState<ResolvedSource>();
  const [taken, setTaken] = useState<string[]>([]);
  const [names, setNames] = useState<string[]>([]);
  const [selected, setSelected] = useState<boolean[]>([]);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState('');
  const [results, setResults] = useState<string[]>([]);
  const [finished, setFinished] = useState(false);
  const resolve = async () => {
    if (busy.current) return;
    busy.current = true; setPending(true); setError(''); setResolved(undefined);
    try {
      const [source, inventory] = await Promise.all([api.resolveImport({ link: link.trim(), path: path.trim() }), api.items()]);
      setTaken(inventory.filter(i => i.item.startsWith('skill:') && i.draft !== 'deleted').map(i => i.item.slice(6)));
      setResolved(source); setNames(source.candidates.map(c => c.default_name));
      setSelected(source.candidates.map(() => source.candidates.length === 1));
      if (!source.candidates.length) setError('No Skills found at this source.');
    } catch (e) { setError(`${errorMessage(e)} Check this import's repository URL and access credentials.`); }
    finally { busy.current = false; setPending(false); }
  };
  const problems = names.map((name, i) => !name ? 'Enter a name.'
    : !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name) ? 'Start with a letter or number; use only letters, numbers, dots, underscores and hyphens.'
    : taken.includes(name) || (resolved?.candidates[i].taken && name === resolved.candidates[i].default_name)
      || names.some((other, j) => j !== i && selected[j] && other === name) ? 'This name is already taken.' : '');
  const add = async () => {
    if (!resolved || busy.current || finished || !selected.some(Boolean) || problems.some((p, i) => selected[i] && p)) return;
    busy.current = true; setPending(true); setError('');
    let current = '';
    try {
      for (let i = 0; i < selected.length; i++) {
        if (!selected[i]) continue;
        current = names[i];
        await api.addImport({ name: names[i], source: resolved.source, path: resolved.candidates[i].path, ref: resolved.git_ref, commit: resolved.commit });
        setResults(r => [...r, `${names[i]} added into Draft.`]);
      }
    } catch (e) {
      setResults(r => [...r, `${current} may not have been added; check the Draft.`]);
      setError(`${errorMessage(e)} Check this import's repository URL and access credentials. Items listed as added stay in the Draft.`);
    }
    finally { busy.current = false; setPending(false); setFinished(true); changed(); }
  };
  return <DialogContent showCloseButton={!pending}>
    <DialogHeader><DialogTitle>Add Import</DialogTitle><DialogDescription>Resolve a link, then add Skills into this Machine's Draft.</DialogDescription></DialogHeader>
    {!resolved && <FieldGroup><Field><FieldLabel htmlFor="import-link">Link</FieldLabel>
      <Input id="import-link" value={link} disabled={pending} onChange={e => setLink(e.target.value)} /></Field>
      <Collapsible><CollapsibleTrigger render={<Button variant="ghost" disabled={pending} />}>Advanced</CollapsibleTrigger>
        <CollapsibleContent><Field><FieldLabel htmlFor="import-path">Path (subdirectory for other git URLs)</FieldLabel>
          <Input id="import-path" value={path} disabled={pending} onChange={e => setPath(e.target.value)} /></Field></CollapsibleContent></Collapsible>
      <Button disabled={pending || !link.trim()} onClick={resolve}>{pending ? 'Resolving…' : 'Resolve'}</Button></FieldGroup>}
    {resolved && <><dl className="text-xs break-all"><dt>Source</dt><dd>{resolved.source}</dd><dt>Ref</dt><dd>{resolved.git_ref}</dd><dt>Commit</dt><dd>{shortSha(resolved.commit)}</dd></dl>
      <FieldGroup>{resolved.candidates.map((candidate, i) => <Field key={candidate.path} data-invalid={selected[i] && !!problems[i]}>
        {resolved.candidates.length > 1 && <FieldLabel><Checkbox aria-label={`Select ${candidate.path || 'root'}`} checked={selected[i]} disabled={pending || finished}
          onCheckedChange={checked => setSelected(s => s.map((v, j) => j === i ? !!checked : v))} />{candidate.path || '(root)'}</FieldLabel>}
        <p>{candidate.description}</p>
        {selected[i] && <><FieldLabel htmlFor={`import-name-${i}`}>Name{resolved.candidates.length > 1 ? ` for ${candidate.path || 'root'}` : ''}</FieldLabel>
          <Input id={`import-name-${i}`} value={names[i]} disabled={pending || finished} aria-invalid={!!problems[i]}
            onChange={e => setNames(n => n.map((v, j) => j === i ? e.target.value : v))} />
          {problems[i] && <p className="text-destructive text-xs">{problems[i]}</p>}</>}
      </Field>)}</FieldGroup></>}
    <ErrorNotice error={error || undefined} />
    <div role="status">{results.map(result => <p key={result}>{result}</p>)}</div>
    <DialogFooter>{finished ? <Button onClick={reviewDraft}>Review in Draft</Button>
      : resolved && <Button disabled={pending || !selected.some(Boolean) || problems.some((p, i) => selected[i] && !!p)} onClick={add}>{pending ? 'Adding…' : 'Add into Draft'}</Button>}</DialogFooter>
  </DialogContent>;
}
