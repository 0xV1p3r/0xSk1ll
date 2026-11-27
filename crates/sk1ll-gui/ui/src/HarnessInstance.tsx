// Adding a preset instance: another config folder of a built-in preset's tool, a Harness of its own.
import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { rehome, suggestName, toolName } from './harnessKinds';
import type { HarnessChange, HarnessChoice } from './types';

// Same rule as sk1ll-core's check_name, as in the Harness dialog.
const namePattern = /^[a-z0-9][a-z0-9_-]{0,63}$/;

// Asks for the folder and a name (suggested from the folder), and whether to start from the preset's Assignment.
export function InstanceDialog({ preset, taken, onClose, onSubmit }: {
  preset: HarnessChoice; taken: string[]; onClose: () => void;
  onSubmit: (change: HarnessChange, copyAssignmentFrom?: string) => void;
}) {
  const [home, setHome] = useState('');
  const [name, setName] = useState('');
  const [named, setNamed] = useState(false);
  const [copy, setCopy] = useState(true);
  const [error, setError] = useState('');
  const tool = toolName(preset.harness.kind ?? preset.name);
  const changeHome = (value: string) => {
    setHome(value);
    if (!named) setName(suggestName(value.trim(), taken));
  };
  const submit = () => {
    const folder = home.trim().replace(/(.)\/+$/, '$1');
    if (!folder.startsWith('~/') && !folder.startsWith('/')) return setError('The config folder must be absolute or start with ~/.');
    if (folder === preset.harness.home) return setError(`${folder} is ${preset.name}'s own config folder; choose another one.`);
    if (!namePattern.test(name)) return setError('Use lowercase letters, digits, - and _ (at most 64), starting with a letter or digit.');
    if (taken.includes(name)) return setError(`${name} already exists; choose another name.`);
    onSubmit({ op: 'set', name, harness: { ...rehome(preset.harness, folder), kind: preset.harness.kind ?? preset.name } }, copy ? preset.name : undefined);
  };
  return <Dialog open onOpenChange={next => { if (!next) onClose(); }}>
    <DialogContent><DialogHeader><DialogTitle>Add another {tool} config folder</DialogTitle>
      <DialogDescription>Another config folder of {tool}, for a second account say, is a Harness of its own with {preset.name}'s settings under that folder. It is saved in machine.toml; the next step shows what sync will do.</DialogDescription></DialogHeader>
      <form onSubmit={e => { e.preventDefault(); submit(); }}><FieldGroup>
        <Field><FieldLabel htmlFor="instance-home">Config folder</FieldLabel><Input id="instance-home" required value={home} onChange={e => changeHome(e.target.value)} placeholder={`${preset.harness.home ?? '~/.tool'}_2`} aria-describedby="instance-home-hint" />
          <FieldDescription id="instance-home-hint">Absolute, or starting with ~/: the folder the tool runs with{preset.name === 'claude' ? ' (CLAUDE_CONFIG_DIR)' : preset.name === 'codex' ? ' (CODEX_HOME)' : preset.name === 'copilot' ? ' (COPILOT_HOME)' : ''}.</FieldDescription></Field>
        <Field><FieldLabel htmlFor="instance-name">Name</FieldLabel><Input id="instance-name" required value={name} onChange={e => { setName(e.target.value); setNamed(true); }} placeholder={`${preset.name}_2`} /></Field>
        <Field orientation="horizontal"><Checkbox id="instance-copy" checked={copy} onCheckedChange={on => setCopy(on)} />
          <FieldLabel htmlFor="instance-copy" className="font-normal">Copy assignment from {preset.name}</FieldLabel></Field>
        <FieldDescription>{copy ? `It starts with ${preset.name}'s Skills and Rules on and off, defaults included.` : 'It starts as any new Harness does (on a Fleet, with everything off).'}</FieldDescription>
        <FieldError>{error}</FieldError>
        <DialogFooter><Button type="button" variant="outline" onClick={onClose}>Cancel</Button><Button type="submit">Preview change</Button></DialogFooter>
      </FieldGroup></form></DialogContent></Dialog>;
}
