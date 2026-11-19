import { useCallback, useState, type ReactNode } from 'react';
import { FileCog, Plus } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Checkbox } from '@/components/ui/checkbox';
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { NativeSelect } from '@/components/ui/native-select';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { toast } from './toast';
import { api, errorStatus } from './api';
import { useMutation, useResource } from './hooks';
import { ErrorNotice, Loading, Pending } from './shared';
import type { HarnessChange, HarnessChoice, HarnessView, PlacementView } from './types';

type HarnessPaths = HarnessView['harness'];

// What the table says about where a Harness comes from.
export function harnessOrigin(h: HarnessChoice): string {
  if (!h.preset) return 'Custom';
  if (h.available && !h.enabled) return h.installed ? 'Opt-in preset' : 'Opt-in preset, not detected';
  if (h.configured) return 'Preset, adjusted';
  return h.installed ? 'Detected' : 'Not detected';
}

// Same rule as sk1ll-core's check_name, so the form can say so before the Daemon does.
export const harnessNamePattern = /^[a-z0-9][a-z0-9_-]{0,63}$/;
const placesPath = (value: string) => value === '' || value.startsWith('~/') || value.startsWith('/');

// The Harnesses of this Machine, switched and edited in its machine.toml. `actions` adds per-Harness controls.
export function HarnessPanel({ revision, editorId, changed, actions }: {
  revision: number; editorId?: string; changed: () => void; actions?: (harness: HarnessChoice) => ReactNode;
}) {
  const settings = useResource(api.harnessSettings, revision);
  const [showAll, setShowAll] = useState(false);
  const [editing, setEditing] = useState<{ name: string; harness: HarnessPaths; existing: boolean }>();
  const [change, setChange] = useState<HarnessChange>();
  const open = useMutation();
  const all = settings.data?.harnesses ?? [];
  const visible = all.filter(h => showAll || h.enabled || h.installed || h.configured);
  const hidden = all.length - visible.length;
  return <div className="flex flex-col gap-3 p-3.5">
    <div className="flex flex-wrap items-start justify-between gap-3"><div><h3 className="text-sm font-medium">Harnesses on this Machine</h3>
      <p className="text-sm text-muted-foreground">Saved in {settings.data ? <code className="font-mono text-xs break-all">{settings.data.path}</code> : <code className="font-mono">machine.toml</code>}. Every change is previewed before it is applied.</p></div>
      <div className="flex gap-2"><Button variant="outline" size="sm" disabled={!editorId || open.pending} title={editorId ? undefined : 'Choose a preferred editor below first'}
        onClick={() => void open.run(() => api.openMachineConfig(editorId!))}><FileCog data-icon="inline-start" /><Pending pending={open.pending}>Open configuration</Pending></Button>
        <Button size="sm" onClick={() => setEditing({ name: '', existing: false, harness: { skills: null, instructions: null, instructions_format: 'block', mode: 'symlink' } })}><Plus data-icon="inline-start" />Add Harness</Button></div></div>
    <ErrorNotice error={settings.error || open.error} />
    {settings.loading && !settings.data && <Loading rows={2} />}
    {settings.data && <div className="overflow-hidden rounded-lg border"><Table aria-label="Harnesses on this Machine"><TableHeader><TableRow className="bg-muted/50 hover:bg-muted/50">
      {['Sync', 'Harness', 'Skills', 'Rules', 'Mode', 'Origin', ''].map((label, i) => <TableHead key={i}>{label}</TableHead>)}
    </TableRow></TableHeader><TableBody>{visible.map(h => <TableRow key={h.name}>
      <TableCell><label className="inline-flex min-h-8 min-w-8 cursor-pointer items-center justify-center"><Checkbox checked={h.enabled} aria-label={`Sync into ${h.name}`}
        onCheckedChange={on => setChange({ op: on ? 'enable' : 'disable', name: h.name })} /></label></TableCell>
      <TableCell className="font-medium">{h.name}</TableCell><TableCell><Path value={h.harness.skills} /></TableCell>
      <TableCell><Path value={h.harness.instructions} />
        <p className="text-xs text-muted-foreground">{h.harness.instructions && (h.harness.instructions_format === 'block' ? 'Single file' : 'Rule files')}</p></TableCell>
      <TableCell>{h.harness.mode === 'symlink' ? 'Symbolic link' : 'Copy'}</TableCell>
      <TableCell><Badge variant="secondary">{harnessOrigin(h)}</Badge>
        {h.available && <p className="text-xs text-muted-foreground">{h.enabled ? 'Its skills directory is available.' : 'Available: off until you enable it.'}</p>}</TableCell>
      <TableCell><div className="flex justify-end gap-1">{actions?.(h)}
        {h.available && <Button variant="ghost" size="sm" aria-label={h.enabled ? `Sync ${h.name}'s skills directory too` : `Enable ${h.name}`} onClick={() => setChange({ op: 'enable', name: h.name })}>{h.enabled ? 'Add skills' : 'Enable'}</Button>}
        <Button variant="ghost" size="sm" aria-label={`Edit ${h.name}`} onClick={() => setEditing({ name: h.name, harness: h.harness, existing: true })}>Edit</Button>
        {h.configured && <Button variant="ghost" size="sm" aria-label={`${h.preset ? 'Reset' : 'Remove'} ${h.name}`} onClick={() => setChange({ op: 'remove', name: h.name })}>{h.preset ? 'Reset' : 'Remove'}</Button>}</div></TableCell>
    </TableRow>)}</TableBody></Table></div>}
    {(hidden > 0 || showAll) && <div><Button variant="ghost" size="sm" onClick={() => setShowAll(value => !value)}>{showAll ? 'Hide presets not found on this Machine' : `Show ${hidden} more preset${hidden === 1 ? '' : 's'}`}</Button></div>}
    {editing && <HarnessDialog initial={editing} preset={all.find(h => h.name === editing.name)?.preset ?? false} taken={all.map(h => h.name)}
      onClose={() => setEditing(undefined)} onSubmit={next => { setEditing(undefined); setChange(next); }} />}
    {change && <ChangeDialog change={change} onClose={() => setChange(undefined)} onApplied={() => { setChange(undefined); void settings.refresh(); changed(); }} />}
  </div>;
}

function HarnessDialog({ initial, preset, taken, onClose, onSubmit }: {
  initial: { name: string; harness: HarnessPaths; existing: boolean }; preset: boolean; taken: string[];
  onClose: () => void; onSubmit: (change: HarnessChange) => void;
}) {
  const [name, setName] = useState(initial.name);
  const [skills, setSkills] = useState(initial.harness.skills ?? '');
  const [instructions, setInstructions] = useState(initial.harness.instructions ?? '');
  const [format, setFormat] = useState(initial.harness.instructions_format);
  const [mode, setMode] = useState(initial.harness.mode);
  const [error, setError] = useState('');
  const submit = () => {
    const [s, i] = [skills.trim(), instructions.trim()];
    if (!initial.existing && !harnessNamePattern.test(name)) return setError('Use lowercase letters, digits, - and _ (at most 64), starting with a letter or digit.');
    if (!initial.existing && taken.includes(name)) return setError(`${name} already exists; edit it instead.`);
    if (!s && !i) return setError('Set a skills directory, an instructions path, or both.');
    if (!placesPath(s) || !placesPath(i)) return setError('Paths must be absolute or start with ~/.');
    onSubmit({ op: 'set', name, harness: { skills: s || null, instructions: i || null, instructions_format: format, mode } });
  };
  return <Dialog open onOpenChange={next => { if (!next) onClose(); }}>
    <DialogContent><DialogHeader><DialogTitle>{initial.existing ? `Edit ${initial.name}` : 'Add a Harness'}</DialogTitle>
      <DialogDescription>{preset ? 'Changes to a preset are saved as an override in machine.toml. Reset returns it to the built-in paths.' : 'A custom Harness is saved in machine.toml.'} The next step shows what sync will do.</DialogDescription></DialogHeader>
      <form onSubmit={e => { e.preventDefault(); submit(); }}><FieldGroup>
        <Field><FieldLabel htmlFor="harness-name">Name</FieldLabel><Input id="harness-name" required value={name} disabled={initial.existing} onChange={e => setName(e.target.value)} placeholder="agents" /></Field>
        <Field><FieldLabel htmlFor="harness-skills">Skills directory</FieldLabel><Input id="harness-skills" value={skills} onChange={e => setSkills(e.target.value)} placeholder="~/.agents/skills" aria-describedby="harness-skills-hint" />
          <FieldDescription id="harness-skills-hint">Absolute, or starting with ~/. Leave empty for none.</FieldDescription></Field>
        <Field><FieldLabel htmlFor="harness-instructions">Instructions</FieldLabel><Input id="harness-instructions" value={instructions} onChange={e => setInstructions(e.target.value)} placeholder="~/.agents/AGENTS.md" /></Field>
        <Field><FieldLabel htmlFor="harness-format">Instructions format</FieldLabel><NativeSelect id="harness-format" value={format} onChange={e => setFormat(e.target.value as HarnessPaths['instructions_format'])}>
          <option value="block">Single file: one marked block</option><option value="rules">Rule files: one file per Rule in a directory</option></NativeSelect></Field>
        <Field><FieldLabel htmlFor="harness-mode">Place skills as</FieldLabel><NativeSelect id="harness-mode" value={mode} onChange={e => setMode(e.target.value as HarnessPaths['mode'])}>
          <option value="symlink">Symbolic links to the repo</option><option value="copy">Marked copies</option></NativeSelect></Field>
        <FieldError>{error}</FieldError>
        <DialogFooter><Button type="button" variant="outline" onClick={onClose}>Cancel</Button><Button type="submit">Preview change</Button></DialogFooter>
      </FieldGroup></form></DialogContent></Dialog>;
}

const changeTitle = (change: HarnessChange) => ({
  enable: `Turn on ${change.name}?`, disable: `Turn off ${change.name}?`, set: `Save ${change.name}?`, remove: `Remove ${change.name} from machine.toml?`,
})[change.op];

// The confirmation step: what sk1ll placed where the change stops syncing (it stays), and what sync does next.
export function ChangeDialog({ change, onClose, onApplied }: { change: HarnessChange; onClose: () => void; onApplied: () => void }) {
  const load = useCallback(() => api.previewHarness({ change }), [change]);
  const preview = useResource(load, 0);
  const [failed, setFailed] = useState<string[]>();
  const apply = useMutation();
  const [warning, setWarning] = useState<string | null>(null);
  const refreshPreview = () => {
    preview.setData(undefined);
    return preview.refresh();
  };
  const turnOff = useMutation(() => void refreshPreview());
  const managed = preview.data?.placed.filter(p => p.kind === 'remove').length ?? 0;
  const edited = preview.data?.placed.filter(p => p.kind === 'conflict').length ?? 0;
  const busy = apply.pending || turnOff.pending;
  return <Dialog open onOpenChange={next => { if (!next && !busy) onClose(); }}>
    <DialogContent showCloseButton={!busy} className="sm:max-w-2xl"><DialogHeader><DialogTitle>{changeTitle(change)}</DialogTitle>
      <DialogDescription>{preview.data ? <>Changes <code className="font-mono text-xs break-all">{preview.data.path}</code>, then syncs. It never removes anything.</> : 'Checking what this change does…'}</DialogDescription></DialogHeader>
      <ErrorNotice error={preview.error} title="This change can't be made" />
      {preview.loading && !preview.data && <Loading rows={2} />}
      {preview.data && <div className="flex max-h-[50vh] flex-col gap-4 overflow-y-auto">
        <section aria-label="Already placed"><h3 className="text-sm font-medium">Already placed by sk1ll</h3>
          {preview.data.warnings?.map(warning => <p key={warning} role="status" className="text-sm text-muted-foreground">{warning}</p>)}
          {preview.data.placed.length === 0 ? !preview.data.warnings?.length && <p className="text-sm text-muted-foreground">{preview.data.before ? 'Nothing that sk1ll placed is left behind by this change.' : 'Nothing: sk1ll does not sync into this Harness yet.'}</p> : <>
            <p className="text-sm text-muted-foreground">These stay where they are, no longer managed by sk1ll.{edited > 0 && ` ${edited} edited by hand.`}</p>
            <Placements rows={preview.data.placed} placed />
            {managed > 0 && preview.data.before && <div className="mt-2 flex flex-col items-start gap-1">
              <Button variant="outline" size="sm" disabled={busy} onClick={() => void turnOff.run(async () => {
                const result = await api.setHarnessAssignment(change.name, false);
                setWarning(result.warning);
                toast({ title: `Turned off ${change.name}'s items`, description: 'Sync removes what it placed there; copies edited by hand stay.' });
              })}><Pending pending={turnOff.pending}>Turn off this Harness's items first</Pending></Button>
              <p className="text-xs text-muted-foreground">Sets every Skill and Rule off for {change.name} in the Assignment, so sync takes out what it placed. Then preview again.</p></div>}</>}
          {warning && <p role="status" className="text-sm text-muted-foreground">{warning}</p>}
          <ErrorNotice error={turnOff.error} />
        </section>
        <section aria-label="Then sync"><h3 className="text-sm font-medium">Then sync</h3>
          {preview.data.awaiting_assignment ? <p className="text-sm text-muted-foreground">No Assignment yet: the Harness is left alone until the Server sends one.</p>
            : preview.data.sync.length === 0 ? <p className="text-sm text-muted-foreground">{preview.data.after ? 'Nothing to place: the Harness already matches.' : 'Nothing: the Harness is no longer synced.'}</p>
            : <Placements rows={preview.data.sync} />}
        </section></div>}
      <ErrorNotice error={apply.error} />
      {failed && <section aria-label="What failed" className="text-sm"><p className="font-medium">machine.toml was changed, but not everything went through:</p>
        <ul className="mt-1 list-disc pl-5 text-destructive">{failed.map(e => <li key={e} className="break-all">{e}</li>)}</ul></section>}
      <DialogFooter>{failed ? <Button onClick={onApplied}>Done</Button> : <>
        <Button variant="outline" disabled={busy} onClick={onClose}>Cancel</Button>
        {(apply.error || preview.error) && !preview.loading && !busy && <Button variant="outline" onClick={() => void refreshPreview()}>Preview again</Button>}
        <Button disabled={!preview.data?.fingerprint || !!preview.error || preview.loading || busy} onClick={() => void apply.run(async () => {
          try {
            return await api.applyHarness({ change, fingerprint: preview.data?.fingerprint });
          } catch (error) {
            if (errorStatus(error) === 409) preview.setData(undefined);
            throw error;
          }
        }, result => {
          if (result.errors.length) return setFailed(result.errors);
          toast({ title: 'Harness changed', description: 'The Harnesses are synced.' });
          onApplied();
        })}><Pending pending={apply.pending}>Apply</Pending></Button></>}</DialogFooter>
    </DialogContent></Dialog>;
}

const kindLabel: Record<PlacementView['kind'], string> = { create: 'Add', update: 'Update', remove: 'Remove', conflict: 'Edited by hand' };

function Placements({ rows, placed = false }: { rows: PlacementView[]; placed?: boolean }) {
  return <ul className="mt-2 flex flex-col gap-1.5">{rows.map(p => <li key={`${p.kind}:${p.dest}`} className="flex flex-wrap items-baseline gap-x-2 text-sm">
    <Badge variant={p.kind === 'conflict' ? 'outline' : 'secondary'}>{placed && p.kind === 'remove' ? 'Placed' : kindLabel[p.kind]}</Badge>
    <span>{p.item === 'instructions' ? `Instructions${p.name ? ` (${p.name})` : ''}` : `${p.item === 'skill' ? 'Skill' : 'Rule'} ${p.name}`}</span>
    <code className="font-mono text-xs break-all text-muted-foreground">{p.dest}</code></li>)}</ul>;
}

export function Path({ value }: { value: string | null }) {
  return value ? <code className="font-mono text-xs break-all">{value}</code> : <span className="text-muted-foreground">Not configured</span>;
}
