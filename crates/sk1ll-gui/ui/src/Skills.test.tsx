// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { TooltipProvider } from '@/components/ui/tooltip';
import { Skills } from './Skills';
import { api } from './api';
import { toast } from './toast';
vi.mock('./toast', () => ({ toast: vi.fn() }));
import type { ContentEntry, ContentType } from './types';

vi.mock('./api', () => ({ api: { skills: vi.fn(), skillTree: vi.fn(), skillFile: vi.fn(), editors: vi.fn(),
  fork: vi.fn(), adoptUnmanaged: vi.fn(), openInEditor: vi.fn(), setPreferredEditor: vi.fn(), createItem: vi.fn(), deleteItem: vi.fn(), renameItem: vi.fn() }, errorMessage: (error: unknown) => String(error) }));
const entry = (name: string, type: ContentType, extra: Partial<ContentEntry> = {}): ContentEntry => ({
  name, item: type === 'unmanaged' ? null : `${type === 'rule' ? 'rule' : 'skill'}:${name}`, type,
  description: `About ${name}`, draft: null, in_baseline: type !== 'unmanaged', path: null,
  enabled_harnesses: ['claude'], clashes_with_repo: false, variants: type === 'unmanaged' ? [
    { hash: 'sha256:one', description: 'First variant', locations: [{ harness: 'claude', path: '/claude/notes' }] },
    { hash: 'sha256:two', description: 'Second variant', locations: [{ harness: 'codex', path: '/codex/notes' }] },
  ] : [], ...extra,
});
const entries = [entry('writing', 'personal'), entry('pdf', 'import'), entry('style', 'rule'), entry('notes', 'unmanaged')];
const changed = vi.fn(), reviewDraft = vi.fn(), assignment = vi.fn(), addImport = vi.fn();
const props = { connection: 'connected' as const, entries, revision: 0, changed, reviewDraft, assignment, addImport };
const markdown = '---\nname: pdf\ndescription: Read PDFs\nharnesses: [claude, codex]\n---\n# Guide\n\nUse **bold** text.\n\n<script>alert(1)</script>';
beforeEach(() => {
  vi.resetAllMocks();
  vi.stubGlobal('matchMedia', vi.fn().mockImplementation(() => ({ matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() })));
  vi.mocked(api.skillTree).mockResolvedValue([{ path: 'SKILL.md', kind: 'file', size: 100 }, { path: 'docs', kind: 'directory', size: 0 }, { path: 'docs/example.md', kind: 'file', size: 30 }]);
  vi.mocked(api.skillFile).mockResolvedValue({ path: 'SKILL.md', text: markdown, binary: false, too_large: false, size: 100 });
  vi.mocked(api.editors).mockResolvedValue({ preferred_editor: 'code', editors: [
    { id: 'code', name: 'VS Code', kind: 'gui' }, { id: 'nvim', name: 'Neovim', kind: 'terminal' }, { id: 'default-app', name: 'Default app', kind: 'system' },
  ] });
  vi.mocked(api.fork).mockResolvedValue(); vi.mocked(api.adoptUnmanaged).mockResolvedValue(); vi.mocked(api.openInEditor).mockResolvedValue();
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
const mount = (values = entries) => render(<TooltipProvider><Skills {...props} entries={values} /></TooltipProvider>);
async function select(name: string, type = 'Import') {
  fireEvent.click(screen.getByRole('row', { name: `View ${type} ${name}` }));
  await screen.findByRole('button', { name: /Fork to edit|Adopt|Open in VS Code/ });
}
it('filters four Types and searches by name in one table', () => {
  mount(); expect(screen.getAllByRole('table')).toHaveLength(1);
  for (const value of entries) {
    fireEvent.change(screen.getByLabelText('Type'), { target: { value: value.type } });
    expect(screen.getAllByRole('row')).toHaveLength(2); expect(screen.getByText(value.name)).toBeTruthy();
  }
  fireEvent.change(screen.getByLabelText('Type'), { target: { value: 'all' } });
  fireEvent.change(screen.getByLabelText('Search by name'), { target: { value: 'PDF' } });
  expect(screen.getAllByRole('row')).toHaveLength(2); expect(screen.getByText('pdf')).toBeTruthy();
});
it('sorts Name and Type and opens rows using the keyboard', async () => {
  mount(); fireEvent.click(screen.getByRole('button', { name: 'Name' }));
  expect(screen.getAllByRole('row')[1].textContent).toContain('writing');
  fireEvent.click(screen.getByRole('button', { name: 'Type' }));
  expect(screen.getAllByRole('row')[1].textContent).toContain('pdf');
  fireEvent.keyDown(screen.getByRole('row', { name: 'View Import pdf' }), { key: 'Enter' });
  expect(await screen.findByRole('button', { name: 'Fork to edit' })).toBeTruthy();
});
it('confirms Fork before opening the Import in the preferred editor', async () => {
  mount(); await select('pdf'); fireEvent.click(screen.getByRole('button', { name: 'Fork to edit' }));
  expect(api.fork).not.toHaveBeenCalled(); expect(screen.getByText(/no longer receive updates from its source/)).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Fork and open' }));
  await waitFor(() => expect(api.openInEditor).toHaveBeenCalledWith('code', 'skill:pdf'));
  expect(api.fork).toHaveBeenCalledWith('pdf');
  expect(vi.mocked(api.fork).mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(api.openInEditor).mock.invocationCallOrder[0]);
  expect(changed).toHaveBeenCalledOnce(); await waitFor(() => expect(api.editors).toHaveBeenCalledTimes(2));
});
it('Adopts the selected variant before opening and passes its hash to browsing', async () => {
  mount(); await select('notes', 'Unmanaged');
  fireEvent.change(screen.getByLabelText('Harness location and variant'), { target: { value: 'sha256:two:codex' } });
  await waitFor(() => expect(api.skillTree).toHaveBeenLastCalledWith({ item: 'unmanaged:notes', hash: 'sha256:two', harness: 'codex' }));
  await waitFor(() => expect(api.skillFile).toHaveBeenLastCalledWith({ item: 'unmanaged:notes', hash: 'sha256:two', harness: 'codex', path: 'SKILL.md' }));
  fireEvent.click(screen.getByRole('button', { name: 'Choose editor' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Adopt and open in VS Code' }));
  expect(screen.getByText(/original is kept in recovery/)).toBeTruthy(); expect(screen.getByText(/selected variant from codex/)).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Adopt and open' }));
  await waitFor(() => expect(api.openInEditor).toHaveBeenCalledWith('code', 'skill:notes'));
  expect(api.adoptUnmanaged).toHaveBeenCalledWith({ name: 'notes', harness: 'codex' });
  expect(vi.mocked(api.adoptUnmanaged).mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(api.openInEditor).mock.invocationCallOrder[0]);
});
it('offers plain Adopt without opening an editor', async () => {
  mount(); await select('notes', 'Unmanaged'); fireEvent.click(screen.getByRole('button', { name: 'Adopt into Draft' }));
  fireEvent.click(within(screen.getByRole('dialog', { name: 'Adopt notes?' })).getByRole('button', { name: 'Adopt into Draft' }));
  await waitFor(() => expect(api.adoptUnmanaged).toHaveBeenCalledOnce()); expect(api.openInEditor).not.toHaveBeenCalled();
});
it('chooses a Terminal editor from the split button and refreshes preference', async () => {
  vi.mocked(api.editors).mockResolvedValueOnce({ preferred_editor: 'code', editors: [{ id: 'code', name: 'VS Code', kind: 'gui' }, { id: 'nvim', name: 'Neovim', kind: 'terminal' }] })
    .mockResolvedValue({ preferred_editor: 'nvim', editors: [{ id: 'nvim', name: 'Neovim', kind: 'terminal' }] });
  mount(); await select('writing', 'Personal'); fireEvent.click(screen.getByRole('button', { name: 'Choose editor' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Neovim' }));
  await waitFor(() => expect(api.openInEditor).toHaveBeenCalledWith('nvim', 'skill:writing'));
  expect(await screen.findByRole('button', { name: 'Open in Neovim' })).toBeTruthy(); expect(api.editors).toHaveBeenLastCalledWith(true);
});
it('preserves the chosen editor through Fork confirmation', async () => {
  mount(); await select('pdf'); fireEvent.click(screen.getByRole('button', { name: 'Choose editor' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: 'Neovim' })); expect(api.fork).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: 'Fork and open' }));
  await waitFor(() => expect(api.openInEditor).toHaveBeenCalledWith('nvim', 'skill:pdf'));
});
it('shows frontmatter separately and toggles Rendered and Raw without raw HTML', async () => {
  mount(); await select('pdf'); expect(await screen.findByRole('heading', { name: 'Guide' })).toBeTruthy();
  expect(screen.getByRole('navigation', { name: 'Files' })).toBeTruthy();
  const header = screen.getByRole('region', { name: 'Frontmatter' });
  expect(within(header).getByText('Read PDFs')).toBeTruthy(); expect(within(header).getByText('["claude","codex"]')).toBeTruthy();
  expect(document.querySelector('script')).toBeNull(); fireEvent.click(screen.getByRole('button', { name: 'Raw' }));
  expect(screen.queryByRole('heading', { name: 'Guide' })).toBeNull(); expect(screen.getByText(/Use \*\*bold\*\* text/).tagName).toBe('PRE');
  expect(screen.getByRole('region', { name: 'Frontmatter' })).toBeTruthy(); fireEvent.click(screen.getByRole('button', { name: 'Rendered' }));
  expect(screen.getByRole('heading', { name: 'Guide' })).toBeTruthy();
});
it('browses a Rule single file and keeps deleted items read-only', async () => {
  vi.mocked(api.skillTree).mockResolvedValue([{ path: 'style.md', kind: 'file', size: 100 }]);
  vi.mocked(api.skillFile).mockResolvedValue({ path: 'style.md', text: '# Style', binary: false, too_large: false, size: 100 });
  mount([entry('style', 'rule', { draft: 'deleted' })]); fireEvent.click(screen.getByRole('row', { name: 'View Rule style' }));
  expect(await screen.findByRole('heading', { name: 'Style' })).toBeTruthy(); expect(api.skillFile).toHaveBeenCalledWith({ item: 'rule:style', path: 'style.md' });
  expect(screen.queryByRole('navigation', { name: 'Files' })).toBeNull();
  expect(screen.queryByRole('button', { name: 'Choose editor' })).toBeNull(); fireEvent.click(screen.getByRole('button', { name: 'Review in Draft' })); expect(reviewDraft).toHaveBeenCalledOnce();
});
it('shows binary and size notices and reloads on revision changes', async () => {
  vi.mocked(api.skillFile).mockResolvedValueOnce({ path: 'SKILL.md', text: null, binary: true, too_large: false, size: 10 })
    .mockResolvedValue({ path: 'SKILL.md', text: null, binary: false, too_large: true, size: 2000000 });
  const view = mount(); await select('pdf'); expect(await screen.findByText('Binary file')).toBeTruthy();
  view.rerender(<TooltipProvider><Skills {...props} revision={1} /></TooltipProvider>); expect(await screen.findByText('File too large')).toBeTruthy();
});
it('keeps editor errors visible after Fork without repeating the conversion', async () => {
  vi.mocked(api.openInEditor).mockRejectedValueOnce('Editor failed');
  mount(); await select('pdf'); fireEvent.click(screen.getByRole('button', { name: 'Fork to edit' })); fireEvent.click(screen.getByRole('button', { name: 'Fork and open' }));
  expect(await screen.findByText('Editor failed')).toBeTruthy(); fireEvent.click(screen.getByRole('button', { name: 'Open in VS Code' }));
  await waitFor(() => expect(api.openInEditor).toHaveBeenCalledTimes(2)); expect(api.fork).toHaveBeenCalledOnce();
});
it('stops after failed Adopt and blocks clashing names', async () => {
  vi.mocked(api.adoptUnmanaged).mockRejectedValueOnce('Cannot Adopt'); mount(); await select('notes', 'Unmanaged');
  fireEvent.click(screen.getByRole('button', { name: 'Choose editor' })); fireEvent.click(await screen.findByRole('menuitem', { name: 'Adopt and open in VS Code' })); fireEvent.click(screen.getByRole('button', { name: 'Adopt and open' }));
  await screen.findAllByText('Cannot Adopt'); expect(api.openInEditor).not.toHaveBeenCalled(); cleanup();
  mount([entry('notes', 'unmanaged', { clashes_with_repo: true })]); await select('notes', 'Unmanaged');
  expect((screen.getByRole('button', { name: 'Adopt into Draft' }) as HTMLButtonElement).disabled).toBe(true);
});
it('Harness chips navigate to the Assignment matrix', () => {
  mount(); fireEvent.click(screen.getByRole('button', { name: 'Assignment for writing in claude' })); expect(assignment).toHaveBeenCalledWith('skill:writing', 'claude'); expect(screen.queryByRole('dialog')).toBeNull();
});

it('refreshes the selected item Type and closes a removed item', async () => {
  const view = mount(); await select('pdf');
  view.rerender(<TooltipProvider><Skills {...props} entries={[entry('pdf', 'personal', { draft: 'modified' })]} revision={1} /></TooltipProvider>);
  expect(await screen.findByRole('button', { name: 'Open in VS Code' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Fork to edit' })).toBeNull();
  view.rerender(<TooltipProvider><Skills {...props} entries={[]} revision={2} /></TooltipProvider>);
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
});
it('does not display the previous file when another file fails to load', async () => {
  vi.mocked(api.skillFile).mockResolvedValueOnce({ path: 'SKILL.md', text: '# Original', binary: false, too_large: false, size: 10 })
    .mockRejectedValueOnce('Cannot read file');
  mount(); await select('pdf'); await screen.findByRole('heading', { name: 'Original' });
  fireEvent.click(screen.getByRole('button', { name: 'example.md' }));
  await screen.findByText('Cannot read file'); expect(screen.queryByRole('heading', { name: 'Original' })).toBeNull();
});
it('does not keep another variant preview when reading the new variant fails', async () => {
  vi.mocked(api.skillFile).mockResolvedValueOnce({ path: 'SKILL.md', text: '# First copy', binary: false, too_large: false, size: 10 })
    .mockRejectedValueOnce('Variant unavailable');
  mount(); await select('notes', 'Unmanaged'); await screen.findByRole('heading', { name: 'First copy' });
  fireEvent.change(screen.getByLabelText('Harness location and variant'), { target: { value: 'sha256:two:codex' } });
  await screen.findByText('Variant unavailable'); expect(screen.queryByRole('heading', { name: 'First copy' })).toBeNull();
});

it.each(['offline', 'connecting', 'auth_rejected', 'revoked'] as const)('disables adoption while %s and explains the connection requirement', async connection => {
  render(<TooltipProvider><Skills {...props} connection={connection} /></TooltipProvider>);
  await select('notes', 'Unmanaged');
  for (const name of ['Adopt into Draft', 'Choose editor']) {
    expect(screen.getByRole('button', { name }).hasAttribute('disabled')).toBe(true);
    fireEvent.click(screen.getByRole('button', { name }));
  }
  expect(screen.getByText('Needs a connection to the Server')).toBeTruthy();
  expect(api.adoptUnmanaged).not.toHaveBeenCalled();
});
it('disables a pending adoption confirmation when the Server disconnects', async () => {
  const view = render(<TooltipProvider><Skills {...props} /></TooltipProvider>);
  await select('notes', 'Unmanaged');
  fireEvent.click(screen.getByRole('button', { name: 'Adopt into Draft' }));
  view.rerender(<TooltipProvider><Skills {...props} connection="offline" /></TooltipProvider>);
  expect(within(screen.getByRole('dialog', { name: 'Adopt notes?' })).getByRole('button', { name: 'Adopt into Draft' }).hasAttribute('disabled')).toBe(true);
});

it('opens an Unmanaged Harness chip at its selected variant without navigating to Assignment', async () => {
  mount(); fireEvent.click(screen.getByRole('button', { name: 'View notes in codex' }));
  expect(await screen.findByRole('dialog')).toBeTruthy();
  expect((screen.getByLabelText('Harness location and variant') as HTMLSelectElement).value).toBe('sha256:two:codex');
  await waitFor(() => expect(api.skillTree).toHaveBeenLastCalledWith({ item: 'unmanaged:notes', hash: 'sha256:two', harness: 'codex' }));
  expect(api.skillFile).toHaveBeenLastCalledWith({ item: 'unmanaged:notes', hash: 'sha256:two', harness: 'codex', path: 'SKILL.md' });
  expect(assignment).not.toHaveBeenCalled();
});
it('offers Add Import for an empty inventory and keeps filtered empty results distinct', () => {
  const view = mount([]);
  expect(screen.getByText('Nothing in the Library yet')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'Add Import' })); expect(addImport).toHaveBeenCalledOnce();
  view.rerender(<TooltipProvider><Skills {...props} /></TooltipProvider>);
  fireEvent.change(screen.getByLabelText('Search by name'), { target: { value: 'missing' } });
  expect(screen.getByText('No matching Skills or Rules')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Add Import' })).toBeNull();
});

it('keeps confirmation focus inside the detail dialog on Cancel and Escape and focuses Review after Adopt', async () => {
  mount(); await select('notes', 'Unmanaged');
  const trigger = screen.getByRole('button', { name: 'Adopt into Draft' });
  for (const close of ['Cancel', 'Escape', 'Adopt into Draft']) {
    trigger.focus(); fireEvent.click(trigger);
    const dialog = await screen.findByRole('dialog', { name: 'Adopt notes?' });
    if (close === 'Escape') fireEvent.keyDown(dialog, { key: 'Escape' });
    else fireEvent.click(within(dialog).getByRole('button', { name: close }));
    const target = close === 'Adopt into Draft' ? await screen.findByRole('button', { name: 'Review in Draft' }) : trigger;
    await waitFor(() => expect(document.activeElement).toBe(target));
    expect(screen.getByRole('dialog', { name: 'notes' }).contains(document.activeElement)).toBe(true);
  }
});

it('explains missing editors', async () => {
  vi.mocked(api.editors).mockResolvedValue({ preferred_editor: '', editors: [] }); mount();
  fireEvent.click(screen.getByRole('row', { name: 'View Personal writing' }));
  expect(await screen.findByText('No editors found')).toBeTruthy(); expect(screen.getByRole('button', { name: 'Open in editor' }).hasAttribute('disabled')).toBe(true);
});

it.each(['skill', 'rule'] as const)('checks collisions only in the %s namespace', async kind => {
  mount();
  fireEvent.click(screen.getByRole('button', { name: 'New' }));
  fireEvent.click(await screen.findByRole('menuitem', { name: kind === 'rule' ? 'New Rule' : 'New Skill' }));
  const input = screen.getByLabelText('Name'), create = screen.getByRole('button', { name: 'Create' });
  for (const name of kind === 'rule' ? ['style'] : ['writing', 'pdf', 'notes']) {
    fireEvent.change(input, { target: { value: name } });
    expect(create.hasAttribute('disabled')).toBe(true);
  }
  fireEvent.change(input, { target: { value: kind === 'rule' ? 'writing' : 'style' } });
  expect(create.hasAttribute('disabled')).toBe(false);
  fireEvent.click(create);
  await waitFor(() => expect(api.createItem).toHaveBeenCalledWith({ kind, name: kind === 'rule' ? 'writing' : 'style', description: undefined }));
});

it('shows the unmanaged handoff notice and switches between Unmanaged and all entries', () => {
  render(<TooltipProvider><Skills {...props} handoff={{ filter: 'unmanaged' }} /></TooltipProvider>);
  expect(screen.getByText(/1 unmanaged skill found in your Harnesses/)).toBeTruthy();
  expect(screen.getAllByRole('row')).toHaveLength(2);
  fireEvent.click(screen.getByRole('button', { name: 'Show all' }));
  expect(screen.getAllByRole('row')).toHaveLength(5);
  fireEvent.click(screen.getByRole('button', { name: 'Show Unmanaged' }));
  expect(screen.getAllByRole('row')).toHaveLength(2);
  expect(screen.getByPlaceholderText('Search Skills and Rules')).toBeTruthy();
});
it('explains an empty name and submits the creation form', async () => {
  mount([]); fireEvent.click(screen.getByRole('button', { name: 'New Skill' }));
  const input = screen.getByLabelText('Name');
  fireEvent.blur(input);
  expect(screen.getByRole('alert').textContent).toBe('Enter a name');
  expect(input.getAttribute('aria-invalid')).toBe('true');
  expect(screen.getByText(/New Skills follow each Harness's default/)).toBeTruthy();
  fireEvent.change(input, { target: { value: 'bad name' } }); expect(screen.getByRole('button', { name: 'Create' }).hasAttribute('disabled')).toBe(true);
  fireEvent.change(input, { target: { value: 'keyboard-skill' } });
  fireEvent.submit(input.closest('form')!);
  await waitFor(() => expect(api.createItem).toHaveBeenCalledWith({ kind: 'skill', name: 'keyboard-skill', description: undefined }));
});
it('opens the matrix without a Harness cell target when none are enabled', async () => {
  mount([entry('writing', 'personal', { enabled_harnesses: [] })]);
  await select('writing', 'Personal');
  fireEvent.click(screen.getByRole('button', { name: 'Open in Assignment matrix' }));
  expect(assignment).toHaveBeenCalledWith('skill:writing', 'all');
});

it.each(['personal', 'rule'] as const)('confirms deletion of a %s item and closes detail on success', async type => {
  vi.mocked(api.deleteItem).mockResolvedValue(); mount([entry('target', type)]); await select('target', type === 'rule' ? 'Rule' : 'Personal');
  fireEvent.click(screen.getByRole('button', { name: 'Delete…' }));
  expect(api.deleteItem).not.toHaveBeenCalled();
  const dialog = screen.getByRole('dialog', { name: 'Delete target?' });
  expect(dialog.textContent).toContain('The deletion reaches other Machines after you commit and push it and they sync.');
  fireEvent.click(within(dialog).getByRole('button', { name: 'Delete' }));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(api.deleteItem).toHaveBeenCalledWith({ kind: type === 'rule' ? 'rule' : 'skill', name: 'target' });
  expect(toast).toHaveBeenCalledWith({ title: 'target deleted in Draft' }); expect(changed).toHaveBeenCalledOnce();
});
it.each(['import', 'unmanaged', 'deleted'] as const)('hides rename and delete for %s entries', async type => {
  mount([entry('target', type === 'deleted' ? 'personal' : type, type === 'deleted' ? { draft: 'deleted' } : {})]);
  fireEvent.click(screen.getByRole('row', { name: `View ${type === 'deleted' ? 'Personal' : type === 'import' ? 'Import' : 'Unmanaged'} target` }));
  expect(await screen.findByRole('dialog')).toBeTruthy();
  expect(screen.queryByRole('button', { name: 'Rename…' })).toBeNull(); expect(screen.queryByRole('button', { name: 'Delete…' })).toBeNull();
});
it.each(['personal', 'rule'] as const)('validates and submits rename for %s, selecting its new name', async type => {
  const kind = type === 'rule' ? 'rule' : 'skill';
  vi.mocked(api.renameItem).mockResolvedValue({ item: `${kind}:fresh`, migrated: ['claude'], assignment_error: null });
  mount([entry('target', type), entry('taken', type), entry('fresh', type === 'rule' ? 'personal' : 'rule')]); await select('target', type === 'rule' ? 'Rule' : 'Personal');
  fireEvent.click(screen.getByRole('button', { name: 'Rename…' }));
  const input = screen.getByLabelText('Name'), submit = screen.getByRole('button', { name: 'Rename' });
  expect(screen.getByText(/Assignment toggles carry over on this Machine only/)).toBeTruthy();
  for (const name of ['', 'target', 'TARGET', 'TAKEN', 'bad name', 'a..b', '_bad']) {
    fireEvent.change(input, { target: { value: name } }); expect(submit.hasAttribute('disabled')).toBe(true);
    fireEvent.submit(input.closest('form')!);
  }
  expect(api.renameItem).not.toHaveBeenCalled();
  fireEvent.change(input, { target: { value: 'fresh' } }); fireEvent.submit(input.closest('form')!);
  expect(await screen.findByRole('dialog', { name: 'fresh' })).toBeTruthy();
  expect(api.renameItem).toHaveBeenCalledWith({ kind, name: 'target', to: 'fresh' });
  expect(toast).toHaveBeenCalledWith({ title: 'Renamed to fresh' }); expect(changed).toHaveBeenCalledOnce();
});
it('reports Assignment copy failure while keeping the rename', async () => {
  vi.mocked(api.renameItem).mockResolvedValue({ item: 'skill:fresh', migrated: [], assignment_error: 'disk full' });
  mount(); await select('writing', 'Personal'); fireEvent.click(screen.getByRole('button', { name: 'Rename…' }));
  const input = screen.getByLabelText('Name'); fireEvent.change(input, { target: { value: 'fresh' } }); fireEvent.submit(input.closest('form')!);
  expect(await screen.findByRole('dialog', { name: 'fresh' })).toBeTruthy();
  expect(toast).toHaveBeenCalledWith({ title: "Renamed, but Assignments weren't copied: disk full", tone: 'destructive' });
});
it('keeps failed rename and delete errors inline', async () => {
  vi.mocked(api.renameItem).mockRejectedValue('Rename failed'); vi.mocked(api.deleteItem).mockRejectedValue('Delete failed');
  mount(); await select('writing', 'Personal'); fireEvent.click(screen.getByRole('button', { name: 'Rename…' }));
  const input = screen.getByLabelText('Name'); fireEvent.change(input, { target: { value: 'fresh' } }); fireEvent.submit(input.closest('form')!);
  expect(await screen.findByText('Rename failed')).toBeTruthy(); fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
  fireEvent.click(screen.getByRole('button', { name: 'Delete…' })); fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
  expect(await screen.findByText('Delete failed')).toBeTruthy(); expect(changed).not.toHaveBeenCalled();
});
it('applies each handoff: a content handoff clears an earlier Type filter, and a create handoff opens the form', async () => {
  const view = render(<TooltipProvider><Skills {...props} handoff={{ filter: 'unmanaged' }} /></TooltipProvider>);
  expect((screen.getByLabelText('Type') as HTMLSelectElement).value).toBe('unmanaged');
  view.rerender(<TooltipProvider><Skills {...props} handoff={{ search: 'pdf' }} /></TooltipProvider>);
  expect((screen.getByLabelText('Type') as HTMLSelectElement).value).toBe('all');
  expect((screen.getByLabelText('Search by name') as HTMLInputElement).value).toBe('pdf');
  view.rerender(<TooltipProvider><Skills {...props} handoff={{ create: 'rule' }} /></TooltipProvider>);
  expect(await screen.findByRole('dialog', { name: 'New Rule' })).toBeTruthy();
});
